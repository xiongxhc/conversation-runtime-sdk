use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{MemoryClock, MemoryStore, MemoryStoreErrorKind};
use conversation_model_adapters::{
    AudioCapture, DiarizationResult, GenerationLanguageModel, NetworkVoiceIoFactory,
    TranscriptionDiarizer, VoiceIoFactory,
};
use conversation_protocol::{
    decode_client_command_with_version, encode_gateway_message_for_version, ClientCommand,
    ClientDiarizedSegment, ClientMediaLeaseDescriptor, ClientMemoryCursor, ClientMemoryInspection,
    ClientMemorySummary, ClientPersonaState, ClientProcessedText, ClientRecordingGap,
    ClientRecordingReconciliation, ClientRecordingSummary, ClientRuntimeError, ClientRuntimeEvent,
    ClientVoiceSessionEvent, GatewayMessage, MemoryApproval, RecoveryDisposition, RuntimeError,
    RuntimeErrorKind, RuntimeEvent, RuntimeProfile, RuntimeStage, RuntimeStatus, SessionId, TurnId,
    VoiceResponseMode, VoiceSessionEvent, CLIENT_PROTOCOL_VERSION, CONTEXT_SEED_PROTOCOL_VERSION,
    DEFAULT_CLIENT_PROTOCOL_VERSION, LEGACY_CLIENT_PROTOCOL_VERSION, MEDIA_PROTOCOL_VERSION,
    RUNTIME_PROFILE_PROTOCOL_VERSION,
};
use conversation_runtime::{
    process_text, read_recording_alignment, reconcile_finalized_recording, reconcile_recording,
    ConversationContext, ProcessedText, RecordingAlignmentItem, RecordingSettings,
    RecordingSummary, SessionRecorder, TextProcessingRequest, TextTurnEventStream, TextTurnRuntime,
    VoiceSessionAdapters, VoiceSessionEventStream, VoiceSessionRuntime,
};
#[cfg(not(test))]
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::memory_extraction::{MemoryExtractedCounts, MemoryExtractionSettings, MemoryExtractor};
use crate::voice_adapters::GatewayVoiceAdapters;
use crate::{FrameError, FrameReader, FrameWriter, MediaLease, MediaServer};

const URGENT_WRITER_BUFFER_SIZE: usize = 2;
const NORMAL_WRITER_BUFFER_SIZE: usize = 4;
const EVENT_WRITER_BUFFER_SIZE: usize = 1;
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
/// Free normal-lane slots a command needs before it is read promptly: a command sends at
/// most two replies there. The voice pump's terminal slot is reserved when its session
/// starts, so no other sender competes for this room.
const COMMAND_REPLY_ROOM: usize = 2;
/// Extra normal-lane capacity for commands read after the bounded wait, so stop, finalize
/// and input EOF are still handled while stdout is stalled.
const COMMAND_OVERFLOW_ROOM: usize = 16;
const COMMAND_REPLY_WAIT: Duration = Duration::from_secs(1);
const INVALID_COMMAND_REQUEST_ID: &str = "invalid-command";
#[cfg(not(test))]
fn next_gateway_session_id() -> SessionId {
    // This provenance boundary also guards a recording handle. A process-local counter would
    // repeat after a restart, so mint it from the operating system instead. Zero is not a valid
    // session identity; retrying that one-in-2^64 draw preserves the existing wire type.
    let random = SystemRandom::new();
    loop {
        let mut bytes = [0_u8; 8];
        random
            .fill(&mut bytes)
            .expect("gateway session randomness was unavailable");
        if let Some(session_id) = std::num::NonZeroU64::new(u64::from_be_bytes(bytes)) {
            return SessionId::new(session_id.get());
        }
    }
}

// The internal command-suite fixtures predate per-connection identities and deliberately use
// literal session id 1. Integration and production builds exercise the live allocator above.
#[cfg(test)]
fn next_gateway_session_id() -> SessionId {
    SessionId::new(1)
}

pub struct GatewaySession {
    protocol_v4: bool,
    control_closed: CancellationToken,
    session_id: SessionId,
    runtime: TextTurnRuntime,
    status: RuntimeStatus,
    memory_inspection: Option<MemoryInspectionAdapters>,
    memory_extraction: Option<Arc<MemoryExtractor>>,
    extracted_counts: Option<mpsc::UnboundedReceiver<MemoryExtractedCounts>>,
    voice: Option<VoiceLane>,
    provider_failure: Option<CancellationToken>,
    recording: Option<RecordingSettings>,
    active_recording: Option<ActiveRecording>,
    media_server: Option<Arc<MediaServer>>,
    media_capture_handoff: Option<mpsc::Sender<Arc<dyn AudioCapture>>>,
    /// The adapter that turns a finished recording into timed text and anonymous clusters.
    /// Selected by the TOML loader and attached by main; absent when no diarizer is configured.
    reconciliation: Option<Arc<dyn TranscriptionDiarizer>>,
    alignment: Option<ActiveAlignmentRead>,
}

struct ActiveAlignmentRead {
    request_id: String,
    receiver: mpsc::Receiver<RecordingAlignmentItem>,
    task: Option<JoinHandle<Result<(), RuntimeError>>>,
    cancellation: CancellationToken,
    last_sent_page: Option<u64>,
    credit: bool,
}

/// The recorder for the voice session that is running, or was the last to run. It outlives the
/// session so a recording can still be finalized after the session has ended.
struct ActiveRecording {
    session_id: SessionId,
    recorder: Arc<Mutex<SessionRecorder>>,
    finalized: bool,
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
    media_lease: Option<ActiveMediaLease>,
}

struct ActiveMediaLease {
    server: Arc<MediaServer>,
    session_id: SessionId,
    _recognition_capture: Arc<dyn AudioCapture>,
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

struct VersionedClientCommand {
    protocol_version: u64,
    command: ClientCommand,
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
        let (normal_sender, normal_receiver) =
            mpsc::channel(NORMAL_WRITER_BUFFER_SIZE + COMMAND_OVERFLOW_ROOM);
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
        Self::with_session_id(runtime, status, next_gateway_session_id())
    }

    /// The live gateway-session identity stamped into its recording headers and commands.
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn with_session_id(
        runtime: TextTurnRuntime,
        status: RuntimeStatus,
        session_id: SessionId,
    ) -> Self {
        Self {
            protocol_v4: false,
            control_closed: CancellationToken::new(),
            session_id,
            runtime,
            status,
            memory_inspection: None,
            memory_extraction: None,
            extracted_counts: None,
            voice: None,
            provider_failure: None,
            recording: None,
            active_recording: None,
            media_server: None,
            media_capture_handoff: None,
            reconciliation: None,
            alignment: None,
        }
    }

    #[cfg(test)]
    fn with_test_session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = session_id;
        self
    }

    /// Pins this private control connection to v4, including its initial ready frame.
    pub fn with_v4_control(mut self) -> Self {
        self.protocol_v4 = true;
        self
    }

    /// Signals transport EOF to inline recovery, which cancels and joins its file/provider work.
    pub fn with_control_closed(mut self, closed: CancellationToken) -> Self {
        self.control_closed = closed;
        self
    }

    /// Records every voice session's capture media under `settings`. Absent — which is what the
    /// default configuration produces — nothing is recorded and the session behaves exactly as
    /// it did before recordings existed.
    pub fn with_recording(mut self, settings: RecordingSettings) -> Self {
        self.recording = Some(settings);
        self
    }

    /// Wires the batch transcription and diarization adapter. Without it a deployment records
    /// meetings but cannot reconcile them, and says so.
    pub fn with_reconciliation(mut self, diarizer: Arc<dyn TranscriptionDiarizer>) -> Self {
        self.reconciliation = Some(diarizer);
        self
    }

    /// When `provider_failure` is cancelled the session tells the client that a
    /// gateway-owned provider exited, then ends.
    pub fn with_provider_failure(mut self, provider_failure: CancellationToken) -> Self {
        self.provider_failure = Some(provider_failure);
        self
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

    /// Configures voice I/O and providers against the text runtime's conversation
    /// context so both lanes always share persona, history, memory, and sequence.
    pub fn with_voice(
        mut self,
        voice: GatewayVoiceAdapters,
        language: Arc<dyn GenerationLanguageModel>,
    ) -> Self {
        let context = self.runtime.context();
        self.voice = Some(VoiceLane {
            adapters: voice,
            context,
            language,
        });
        self
    }

    /// Gives this control session the gateway-owned media capability. The caller
    /// keeps the owning handle and is responsible for its final shutdown.
    pub fn with_media_server(mut self, media_server: Arc<MediaServer>) -> Self {
        self.media_server = Some(media_server);
        self
    }

    /// Supplies the production-owned recognizer capture to the later online recognizer wiring.
    /// The gateway attaches it before accepting the lease, while Task 7 remains responsible for
    /// selecting and starting a provider against this exact receiver.
    pub fn with_media_capture_handoff(
        mut self,
        handoff: mpsc::Sender<Arc<dyn AudioCapture>>,
    ) -> Self {
        self.media_capture_handoff = Some(handoff);
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
        let provider_failure = self.provider_failure.clone().unwrap_or_default();
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
        // Explicit v4 remains pinned. Default startup still announces v3 and preserves the
        // legacy per-command response policy, including the minimum truthful status version.
        let minimum_version = minimum_status_version(&self.status);
        let initial_version = if self.protocol_v4 {
            CLIENT_PROTOCOL_VERSION
        } else {
            DEFAULT_CLIENT_PROTOCOL_VERSION
        };
        let negotiated_version = Arc::new(AtomicU64::new(initial_version));
        let mut writer_task = tokio::spawn(writer_loop(
            writer,
            urgent_receiver,
            normal_receiver,
            event_receiver,
            terminal_written,
            Arc::clone(&negotiated_version),
        ));
        let mut active: Option<ActiveForwarder> = None;
        let mut voice = VoiceSessionState {
            active: None,
            terminals_enqueued: 0,
            terminals_written: 0,
        };

        let exit = if let Err(error) = send_urgent(
            &urgent_sender,
            if self.protocol_v4 {
                GatewayMessage::ReadyV4 {
                    status: self.status.clone(),
                    current_session_id: self.session_id,
                }
            } else {
                GatewayMessage::Ready {
                    status: self.status.clone(),
                }
            },
        ) {
            SessionExit::failure(error)
        } else {
            let mut provider_failed = false;
            loop {
                // A provider-host exit degrades recognition and synthesis only. The session ends
                // with it once no voice session is recording and its recording is finalized.
                if provider_failed
                    && voice.active.is_none()
                    && self
                        .active_recording
                        .as_ref()
                        .is_none_or(|recording| recording.finalized)
                {
                    break SessionExit::ProviderFailure;
                }
                let next = if let Some(alignment) = self.alignment.as_mut() {
                    tokio::select! {biased;
                        writer=&mut writer_task=>SessionInput::Writer(writer),
                        _=provider_failure.cancelled(),if !provider_failed=>SessionInput::ProviderFailure,
                        result=async {alignment.task.as_mut().expect("guarded task").await},if alignment.task.is_some()=>SessionInput::AlignmentReader(result),
                        item=alignment.receiver.recv(),if alignment.credit=>SessionInput::AlignmentItem(item),
                        frame=read_frame_with_reply_room(&mut reader,&normal_sender)=>SessionInput::Frame(frame),
                    }
                } else if let Some(active_task) = active
                    .as_mut()
                    .and_then(|active_turn| active_turn.task.as_mut())
                {
                    tokio::select! {
                        biased;
                        terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                        writer = &mut writer_task => SessionInput::Writer(writer),
                        _ = provider_failure.cancelled(), if !provider_failed => SessionInput::ProviderFailure,
                        forwarding = active_task => SessionInput::Forwarder(forwarding),
                        frame = read_frame_with_reply_room(&mut reader, &normal_sender) => SessionInput::Frame(frame),
                    }
                } else if let Some(voice_session) = voice.active.as_mut() {
                    let voice_task = &mut voice_session.task;
                    if let Some(control) = voice_session.control.as_mut() {
                        tokio::select! {
                            biased;
                            terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                            writer = &mut writer_task => SessionInput::Writer(writer),
                        _ = provider_failure.cancelled(), if !provider_failed => SessionInput::ProviderFailure,
                            pumped = voice_task => SessionInput::VoicePump(pumped),
                            controlled = &mut control.task => SessionInput::VoiceControl(controlled),
                            frame = read_frame_with_reply_room(&mut reader, &normal_sender) => SessionInput::Frame(frame),
                        }
                    } else {
                        tokio::select! {
                            biased;
                            terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                            writer = &mut writer_task => SessionInput::Writer(writer),
                        _ = provider_failure.cancelled(), if !provider_failed => SessionInput::ProviderFailure,
                            pumped = voice_task => SessionInput::VoicePump(pumped),
                            frame = read_frame_with_reply_room(&mut reader, &normal_sender) => SessionInput::Frame(frame),
                        }
                    }
                } else {
                    tokio::select! {
                        biased;
                        terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                        writer = &mut writer_task => SessionInput::Writer(writer),
                        _ = provider_failure.cancelled(), if !provider_failed => SessionInput::ProviderFailure,
                        frame = read_frame_with_reply_room(&mut reader, &normal_sender) => SessionInput::Frame(frame),
                    }
                };

                match next {
                    SessionInput::AlignmentReader(result) => {
                        let alignment = self.alignment.as_mut().expect("active alignment task");
                        alignment.task.take();
                        if let Err(error) = result.unwrap_or_else(|_| {
                            Err(RuntimeError::new(
                                RuntimeErrorKind::Adapter,
                                RuntimeStage::AudioCapture,
                                "recording alignment reader did not finish",
                            ))
                        }) {
                            let request_id = alignment.request_id.clone();
                            shutdown_alignment(&mut self.alignment).await;
                            if let Err(error) = send_rejection(
                                &normal_sender,
                                &request_id,
                                ClientRuntimeError::from(error),
                            ) {
                                break SessionExit::failure(error);
                            }
                        }
                    }
                    SessionInput::AlignmentItem(Some(item)) => {
                        let alignment = self.alignment.as_mut().expect("active alignment read");
                        let request_id = alignment.request_id.clone();
                        let message = match item {
                            RecordingAlignmentItem::Page(page) => {
                                alignment.last_sent_page =
                                    Some(page.page_index.parse().expect("validated index"));
                                alignment.credit = false;
                                GatewayMessage::RecordingAlignmentPage { request_id, page }
                            }
                            RecordingAlignmentItem::Complete(completion) => {
                                // Completion is sent only after the blocking reader has exited.
                                if let Some(task) = alignment.task.take() {
                                    match task.await {
                                        Ok(Ok(())) => {}
                                        _ => {
                                            shutdown_alignment(&mut self.alignment).await;
                                            if let Err(error) = send_rejection(
                                                &normal_sender,
                                                &request_id,
                                                command_error("recording alignment cleanup failed"),
                                            ) {
                                                break SessionExit::failure(error);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                self.alignment.take();
                                GatewayMessage::RecordingAlignmentComplete {
                                    request_id,
                                    completion,
                                }
                            }
                        };
                        if let Err(error) = send_normal(&normal_sender, message) {
                            break SessionExit::failure(error);
                        }
                    }
                    SessionInput::AlignmentItem(None) => {
                        let request_id = self
                            .alignment
                            .as_ref()
                            .expect("active alignment read")
                            .request_id
                            .clone();
                        shutdown_alignment(&mut self.alignment).await;
                        if let Err(error) = send_rejection(
                            &normal_sender,
                            &request_id,
                            command_error("recording alignment ended without completion"),
                        ) {
                            break SessionExit::failure(error);
                        }
                    }
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
                            revoke_media_lease(voice_session.media_lease).await;
                            break SessionExit::failure(error);
                        }
                        revoke_media_lease(voice_session.media_lease).await;
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
                        let (version, command) = match decode_client_command_with_version(&payload)
                        {
                            Ok((version, command)) if (version == 4) == self.protocol_v4 => {
                                negotiated_version
                                    .store(version.max(minimum_version), Ordering::Relaxed);
                                (version, command)
                            }
                            _ => {
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
                                VersionedClientCommand {
                                    protocol_version: version,
                                    command,
                                },
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
                    SessionInput::Frame(Ok(None)) => {
                        break if provider_failed {
                            SessionExit::ProviderFailure
                        } else {
                            SessionExit::Normal
                        };
                    }
                    SessionInput::ProviderFailure => provider_failed = true,
                    SessionInput::Frame(Err(error)) => {
                        break SessionExit::fatal(
                            GatewaySessionError::Framing(error),
                            "gateway input framing failed",
                        );
                    }
                }
            }
        };

        shutdown_alignment(&mut self.alignment).await;
        shutdown_active(&self.runtime, &mut active).await;
        shutdown_active_voice(&mut voice.active).await;
        // A recording the client never finalized is closed rather than left open on disk.
        if let Some(active) = self.active_recording.as_ref() {
            let _ = active.recorder.lock().await.finalize();
        }
        if let Some(message) = exit.fatal_message() {
            let _ = send_urgent(&urgent_sender, fatal_message(message));
        }
        if matches!(exit, SessionExit::ProviderFailure) {
            // Behind every reply already queued on the normal lane: a deferred exit follows
            // the reply that finalized the recording.
            let _ = timeout(
                WRITER_SHUTDOWN_TIMEOUT,
                normal_sender.send(provider_failure_message()),
            )
            .await;
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
            SessionExit::Normal | SessionExit::ProviderFailure => writer_shutdown,
            SessionExit::Failure { error, .. } | SessionExit::Writer(error) => {
                let _ = writer_shutdown;
                Err(error)
            }
        }
    }

    async fn handle_command(
        &mut self,
        versioned_command: VersionedClientCommand,
        urgent: &mpsc::Sender<GatewayMessage>,
        normal: &mpsc::Sender<GatewayMessage>,
        events: &mpsc::Sender<GatewayMessage>,
        active: &mut Option<ActiveForwarder>,
        voice: &mut VoiceSessionState,
    ) -> Result<(), CommandFailure> {
        let VersionedClientCommand {
            protocol_version,
            command,
        } = versioned_command;
        if self
            .alignment
            .as_ref()
            .is_some_and(|alignment| alignment.request_id == command_request_id(&command))
            && !matches!(
                command,
                ClientCommand::RecordingAlignmentCredit { .. }
                    | ClientCommand::CancelRecordingAlignment { .. }
            )
        {
            let request_id = command_request_id(&command).to_owned();
            shutdown_alignment(&mut self.alignment).await;
            return send_rejection(
                normal,
                &request_id,
                command_error("recording alignment request identifier was reused"),
            )
            .map_err(CommandFailure::response);
        }
        if self.alignment.is_some()
            && !matches!(
                command,
                ClientCommand::RecordingAlignmentCredit { .. }
                    | ClientCommand::CancelRecordingAlignment { .. }
                    | ClientCommand::Status { .. }
            )
        {
            return send_rejection(
                normal,
                command_request_id(&command),
                command_error("recording alignment requires exclusive idle admission"),
            )
            .map_err(CommandFailure::response);
        }
        // Both timed starts share one start implementation; only the requested
        // media version differs and binds the reservation, codec and lease.
        let (timed_bridge, drainable, command) = match command {
            ClientCommand::StartTimedVoiceSession {
                request_id,
                bridge_session_id,
                profile,
                response_mode,
            } => (
                Some(bridge_session_id),
                false,
                ClientCommand::StartVoiceSession {
                    request_id,
                    profile: Some(profile),
                    response_mode: Some(response_mode),
                },
            ),
            ClientCommand::StartDrainableTimedVoiceSession {
                request_id,
                bridge_session_id,
                profile,
                response_mode,
            } => (
                Some(bridge_session_id),
                true,
                ClientCommand::StartVoiceSession {
                    request_id,
                    profile: Some(profile),
                    response_mode: Some(response_mode),
                },
            ),
            other => (None, false, other),
        };
        match command {
            ClientCommand::ReadRecordingAlignment {
                request_id,
                capability,
            } => {
                if voice.active.is_some() || active.is_some() || voice.terminal_pending() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("recording alignment requires an idle session"),
                    )
                    .map_err(CommandFailure::response);
                }
                let Some(settings) = self.recording.clone() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("this deployment does not record sessions"),
                    )
                    .map_err(CommandFailure::response);
                };
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                let (sender, receiver) = mpsc::channel(1);
                let cancellation = CancellationToken::new();
                let task = tokio::spawn(read_recording_alignment(
                    settings,
                    capability,
                    sender,
                    cancellation.clone(),
                ));
                self.alignment = Some(ActiveAlignmentRead {
                    request_id,
                    receiver,
                    task: Some(task),
                    cancellation,
                    last_sent_page: None,
                    credit: true,
                });
                Ok(())
            }
            ClientCommand::RecordingAlignmentCredit {
                request_id,
                consumed_page_index,
            } => {
                let Some(alignment) = self
                    .alignment
                    .as_mut()
                    .filter(|alignment| alignment.request_id == request_id)
                else {
                    return Ok(());
                };
                if alignment.credit || alignment.last_sent_page != Some(consumed_page_index) {
                    shutdown_alignment(&mut self.alignment).await;
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("recording alignment credit was invalid"),
                    )
                    .map_err(CommandFailure::response);
                }
                alignment.credit = true;
                Ok(())
            }
            ClientCommand::CancelRecordingAlignment { request_id } => {
                if self
                    .alignment
                    .as_ref()
                    .is_some_and(|alignment| alignment.request_id == request_id)
                {
                    shutdown_alignment(&mut self.alignment).await;
                    send_rejection(
                        normal,
                        &request_id,
                        command_error("recording alignment was cancelled"),
                    )
                    .map_err(CommandFailure::response)?;
                }
                Ok(())
            }
            ClientCommand::StartTimedVoiceSession { .. }
            | ClientCommand::StartDrainableTimedVoiceSession { .. } => {
                unreachable!("normalized timed start")
            }
            ClientCommand::RenewMediaLease { request_id } => {
                let Some(lease) = voice
                    .active
                    .as_ref()
                    .and_then(|voice_session| voice_session.media_lease.as_ref())
                else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no media lease is active"),
                    )
                    .map_err(CommandFailure::response);
                };
                match lease.server.renew(lease.session_id).await {
                    Ok(expires_at_ms) => send_accepted(normal, &request_id).and_then(|()| {
                        send_normal(
                            normal,
                            GatewayMessage::MediaLeaseRenewed {
                                request_id,
                                expires_at_ms,
                            },
                        )
                    }),
                    Err(_) => send_rejection(
                        normal,
                        &request_id,
                        command_error("the media lease can no longer be renewed"),
                    ),
                }
                .map_err(CommandFailure::response)
            }
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
            ClientCommand::SeedConversationContext {
                request_id,
                operation_id,
                exchanges,
            } => {
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error(
                            "conversation history cannot be seeded while a text turn is active",
                        ),
                    )
                    .map_err(CommandFailure::response);
                }
                if voice.terminal_pending() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error(
                            "conversation history cannot be seeded while a voice session is finishing",
                        ),
                    )
                    .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error(
                            "conversation history cannot be seeded while a voice session is active",
                        ),
                    )
                    .map_err(CommandFailure::response);
                }
                match self
                    .runtime
                    .context()
                    .seed_completed_history(&operation_id, &exchanges)
                    .await
                {
                    Ok(()) => {
                        self.status.last_context_seed_operation_id = Some(operation_id);
                        send_accepted(normal, &request_id).map_err(CommandFailure::response)
                    }
                    Err(error) => {
                        send_rejection(normal, &request_id, ClientRuntimeError::from(error))
                            .map_err(CommandFailure::response)
                    }
                }
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
            ClientCommand::StartVoiceSession {
                request_id,
                profile,
                response_mode,
            } => {
                if timed_bridge.is_some()
                    && (self.recording.is_none()
                        || self
                            .media_server
                            .as_ref()
                            .and_then(|server| server.capture_format())
                            .is_none())
                {
                    return send_rejection(normal,&request_id,command_error("timed capture requires network media, recording and explicit capture format")).map_err(CommandFailure::response);
                }
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
                if self
                    .provider_failure
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a gateway-owned provider process exited"),
                    )
                    .map_err(CommandFailure::response);
                }
                // The next recording replaces the only reference to this one's handle, so the
                // client finalizes it — and learns its handle — first.
                if let Some(previous) = self.active_recording.as_ref() {
                    if !previous.recorder.lock().await.is_finalized() {
                        return send_rejection(
                            normal,
                            &request_id,
                            command_error("the previous recording has not been finalized"),
                        )
                        .map_err(CommandFailure::response);
                    }
                }
                if let Some(profile) = profile {
                    if RuntimeProfile::parse(&self.status.profile) != Some(profile) {
                        return send_rejection(
                            normal,
                            &request_id,
                            command_error("requested profile is unavailable"),
                        )
                        .map_err(CommandFailure::response);
                    }
                }

                let session_id = self.session_id;
                let policy = match lane.adapters.policy.for_session(session_id) {
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
                // The key is resolved here, before any media flows. A recording that cannot be
                // protected refuses the session rather than starting an unprotected one.
                let recorder = match self.recording.as_ref() {
                    Some(settings) => match if timed_bridge.is_some() {
                        SessionRecorder::start_timed(settings, session_id)
                    } else {
                        SessionRecorder::start(settings, session_id)
                    } {
                        Ok(recorder) => Some(Arc::new(Mutex::new(recorder))),
                        Err(error) => {
                            return send_rejection(
                                normal,
                                &request_id,
                                ClientRuntimeError::from(error),
                            )
                            .map_err(CommandFailure::response);
                        }
                    },
                    None => None,
                };
                let (media_lease, lease_descriptor, io) = match self.media_server.as_ref() {
                    Some(media_server) => {
                        if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                            return send_rejection(
                                normal,
                                &request_id,
                                command_error("configured media requires protocol version 3"),
                            )
                            .map_err(CommandFailure::response);
                        }
                        let lease = match if let Some(bridge) = &timed_bridge {
                            if drainable {
                                media_server
                                    .reserve_drainable_timed(session_id, bridge.clone())
                                    .await
                            } else {
                                media_server.reserve_timed(session_id, bridge.clone()).await
                            }
                        } else {
                            media_server.reserve(session_id).await
                        } {
                            Ok(lease) => lease,
                            Err(_) => {
                                return send_rejection(
                                    normal,
                                    &request_id,
                                    command_error("media session could not be reserved"),
                                )
                                .map_err(CommandFailure::response)
                            }
                        };
                        let descriptor = match media_lease_descriptor(media_server, &lease)
                            .and_then(|descriptor| {
                                if timed_bridge.is_some() {
                                    let capture_format = media_server
                                        .capture_format()
                                        .expect("timed admission")
                                        .to_owned();
                                    if drainable {
                                        descriptor.into_drainable_timed(capture_format)
                                    } else {
                                        descriptor.into_timed(capture_format)
                                    }
                                    .map_err(|_| ())
                                } else {
                                    Ok(descriptor)
                                }
                            }) {
                            Ok(descriptor) => descriptor,
                            Err(()) => {
                                media_server.revoke(session_id).await;
                                return send_rejection(
                                    normal,
                                    &request_id,
                                    command_error("media lease could not be prepared"),
                                )
                                .map_err(CommandFailure::response);
                            }
                        };
                        let network_factory =
                            Arc::new(NetworkVoiceIoFactory::new(media_server.hub(), session_id));
                        let recognition_capture =
                            match network_factory.attach_recognition_capture().await {
                                Ok(capture) => capture,
                                Err(_) => {
                                    media_server.revoke(session_id).await;
                                    return send_rejection(
                                        normal,
                                        &request_id,
                                        command_error(
                                            "media recognition capture could not be attached",
                                        ),
                                    )
                                    .map_err(CommandFailure::response);
                                }
                            };
                        if let Some(handoff) = self.media_capture_handoff.as_ref() {
                            if handoff.try_send(Arc::clone(&recognition_capture)).is_err() {
                                media_server.revoke(session_id).await;
                                return send_rejection(
                                    normal,
                                    &request_id,
                                    command_error(
                                        "media recognition capture could not be handed off",
                                    ),
                                )
                                .map_err(CommandFailure::response);
                            }
                        }
                        let media_io = network_factory.clone() as Arc<dyn VoiceIoFactory>;
                        let online_profile = RuntimeProfile::parse(&self.status.profile)
                            == Some(RuntimeProfile::Online);
                        let io = if online_profile {
                            match lane
                                .adapters
                                .online_media_io(media_io.clone(), Arc::clone(&recognition_capture))
                            {
                                Some(io) => io,
                                None => {
                                    media_server.revoke(session_id).await;
                                    return send_rejection(
                                        normal,
                                        &request_id,
                                        ClientRuntimeError::from(RuntimeError::new(
                                            RuntimeErrorKind::Adapter,
                                            RuntimeStage::SpeechRecognizer,
                                            "online speech recognition could not be configured",
                                        )),
                                    )
                                    .map_err(CommandFailure::response);
                                }
                            }
                        } else {
                            media_io
                        };
                        (
                            Some(ActiveMediaLease {
                                server: Arc::clone(media_server),
                                session_id,
                                _recognition_capture: recognition_capture,
                            }),
                            Some(descriptor),
                            io,
                        )
                    }
                    None => (None, None, lane.adapters.io.clone()),
                };
                let mut adapters = VoiceSessionAdapters::new(
                    io,
                    lane.language.clone(),
                    lane.adapters.speech.clone(),
                );
                if let Some(recorder) = recorder.as_ref() {
                    adapters = adapters.with_recorder(Arc::clone(recorder));
                }
                let runtime = VoiceSessionRuntime::new(lane.context.clone(), adapters);
                let mut event_stream = match runtime
                    .start_with_response_mode(
                        policy,
                        response_mode.unwrap_or(VoiceResponseMode::Automatic),
                    )
                    .await
                {
                    Ok(event_stream) => event_stream,
                    Err(error) => {
                        revoke_media_lease(media_lease).await;
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                if media_lease.is_some()
                    && event_stream.wait_until_started(session_id).await.is_err()
                {
                    let _ = runtime.shutdown().await;
                    revoke_media_lease(media_lease).await;
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("media voice session could not be started"),
                    )
                    .map_err(CommandFailure::response);
                }

                let accepted = GatewayMessage::CommandAccepted {
                    request_id: request_id.clone(),
                    turn_id: None,
                    media_lease: if timed_bridge.is_some() {
                        None
                    } else {
                        lease_descriptor.clone()
                    },
                };
                // The acceptance goes on the urgent lane for every start: the pump spawned
                // below puts `voice_session_started` on the event lane, which the writer can
                // prefer over the normal lane, and clients open the voice session only on
                // this acceptance. The timed start may follow that first event.
                // Reserved before any reply so the pump's terminal never competes with
                // command replies for the room the reader guaranteed.
                let Ok(terminal_slot) = normal.clone().try_reserve_owned() else {
                    let _ = runtime.shutdown().await;
                    revoke_media_lease(media_lease).await;
                    return Err(CommandFailure::response(
                        GatewaySessionError::WriterBackpressure,
                    ));
                };
                let acceptance = if timed_bridge.is_some() {
                    send_urgent(urgent, accepted).and_then(|()| {
                        send_normal(
                            normal,
                            GatewayMessage::TimedVoiceSessionStarted {
                                request_id: request_id.clone(),
                                media_lease: lease_descriptor
                                    .expect("timed admission supplied lease"),
                            },
                        )
                    })
                } else {
                    send_urgent(urgent, accepted)
                };
                if let Err(error) = acceptance {
                    let _ = runtime.shutdown().await;
                    revoke_media_lease(media_lease).await;
                    return Err(CommandFailure::response(error));
                }
                let (control_events, control_event_receiver) = mpsc::channel(1);
                let (control_pending, control_pending_receiver) = watch::channel(false);
                let task = tokio::spawn(pump_voice_events(
                    event_stream,
                    self.memory_extraction.clone(),
                    terminal_slot,
                    events.clone(),
                    control_event_receiver,
                    control_pending_receiver,
                    VoiceNotices::new(
                        session_id,
                        media_lease.as_ref(),
                        self.provider_failure.clone(),
                    ),
                ));
                voice.active = Some(ActiveVoiceSession {
                    runtime,
                    task,
                    control: None,
                    control_events,
                    control_pending,
                    capture: VoiceCaptureState::Active,
                    media_lease,
                });
                if let Some(recorder) = recorder {
                    self.active_recording = Some(ActiveRecording {
                        session_id,
                        recorder,
                        finalized: false,
                    });
                }
                Ok(())
            }
            ClientCommand::AuthorizeVoiceResponse {
                request_id,
                session_id,
                segment_id,
                context_exchanges,
            } => {
                let Some(voice_session) = voice.active.as_ref() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                };
                match voice_session
                    .runtime
                    .authorize_response(session_id, segment_id, context_exchanges)
                    .await
                {
                    Ok(turn_id) => send_normal(normal, accepted_turn_message(&request_id, turn_id))
                        .map_err(CommandFailure::response),
                    Err(error) => {
                        send_rejection(normal, &request_id, ClientRuntimeError::from(error))
                            .map_err(CommandFailure::response)
                    }
                }
            }
            ClientCommand::SkipVoiceResponse {
                request_id,
                session_id,
                segment_id,
                reason,
            } => {
                let Some(voice_session) = voice.active.as_ref() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                };
                match voice_session
                    .runtime
                    .skip_response(session_id, segment_id, reason)
                    .await
                {
                    Ok(()) => send_accepted(normal, &request_id).map_err(CommandFailure::response),
                    Err(error) => {
                        send_rejection(normal, &request_id, ClientRuntimeError::from(error))
                            .map_err(CommandFailure::response)
                    }
                }
            }
            ClientCommand::FinalizeRecording {
                request_id,
                session_id,
            } => {
                // Three different things used to share one diagnostic. A client cannot act on
                // "no recording available" without knowing which of them it hit.
                if self.recording.is_none() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("this deployment does not record sessions"),
                    )
                    .map_err(CommandFailure::response);
                }
                // Finalizing mid-session would close the sink under the running media path and
                // strand the rest of the meeting's audio, so the recording is closed only once
                // the session that feeds it has stopped.
                if voice.active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("the voice session is still recording"),
                    )
                    .map_err(CommandFailure::response);
                }
                let Some(active) = self
                    .active_recording
                    .as_ref()
                    .filter(|active| active.session_id == session_id)
                else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no recording exists for that session"),
                    )
                    .map_err(CommandFailure::response);
                };
                let mut recorder = active.recorder.lock().await;
                let finalized = if self.protocol_v4 {
                    let finalized = recorder
                        .finalize_with_capability()
                        .map(|(summary, capability)| (summary, Some(capability)));
                    // An incomplete recording can never be issued a capability. Close it anyway,
                    // so the file is complete on disk and the session can record again.
                    if finalized.is_err() && recorder.is_incomplete() && !recorder.is_finalized() {
                        let _ = recorder.finalize();
                    }
                    finalized
                } else {
                    recorder.finalize().map(|summary| (summary, None))
                };
                drop(recorder);
                let (recording, recovery_capability) = match finalized {
                    Ok((summary, capability)) => {
                        if let Some(active) = self.active_recording.as_mut() {
                            active.finalized = true;
                        }
                        (client_recording_summary(&summary), capability)
                    }
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    if let Some(recovery_capability) = recovery_capability {
                        GatewayMessage::RecordingFinalizedV4 {
                            request_id,
                            recording,
                            recovery_capability,
                        }
                    } else {
                        GatewayMessage::RecordingFinalized {
                            request_id,
                            recording,
                        }
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::ReconcileRecording {
                request_id,
                session_id,
                recording_handle,
            } => {
                if session_id != self.session_id {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("recording reconciliation was requested for another session"),
                    )
                    .map_err(CommandFailure::response);
                }
                let Some(settings) = self.recording.clone() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("this deployment does not record sessions"),
                    )
                    .map_err(CommandFailure::response);
                };
                let Some(diarizer) = self.reconciliation.clone() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("this deployment does not reconcile recordings"),
                    )
                    .map_err(CommandFailure::response);
                };
                // Reconciliation is post-meeting work. Reading a file the sink is still
                // appending to would answer for a prefix of the meeting and call it the whole.
                if voice.active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("the voice session is still recording"),
                    )
                    .map_err(CommandFailure::response);
                }
                // The handle's random per-recording binding prevents substitution inside a
                // recording directory, and the live gateway-session identity in the header
                // prevents a known handle from crossing between concurrent gateway sessions.
                let reconciled = reconcile_recording(
                    &settings,
                    session_id,
                    &recording_handle,
                    diarizer,
                    self.provider_failure.clone().unwrap_or_default(),
                )
                .await;
                let reconciliation = match reconciled {
                    Ok(result) => client_reconciliation(&result),
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                send_reconciliation(normal, request_id, reconciliation, protocol_version)
                    .map_err(CommandFailure::response)
            }
            ClientCommand::ReconcileFinalizedRecording {
                request_id,
                capability,
            } => {
                if voice.active.is_some() || active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("recording recovery requires an idle session"),
                    )
                    .map_err(CommandFailure::response);
                }
                let (Some(settings), Some(diarizer)) =
                    (self.recording.as_ref(), self.reconciliation.clone())
                else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("this deployment does not reconcile recordings"),
                    )
                    .map_err(CommandFailure::response);
                };
                let cancellation = self
                    .provider_failure
                    .clone()
                    .unwrap_or_default()
                    .child_token();
                let recovery = reconcile_finalized_recording(
                    settings,
                    &capability,
                    diarizer,
                    cancellation.clone(),
                );
                tokio::pin!(recovery);
                let result = tokio::select! {
                    result = &mut recovery => result,
                    _ = self.control_closed.cancelled() => {
                        cancellation.cancel();
                        recovery.await
                    }
                };
                match result {
                    Ok(result) => send_reconciliation(
                        normal,
                        request_id,
                        client_reconciliation(&result),
                        protocol_version,
                    )
                    .map_err(CommandFailure::response),
                    Err(error) => {
                        send_rejection(normal, &request_id, ClientRuntimeError::from(error))
                            .map_err(CommandFailure::response)
                    }
                }
            }
            ClientCommand::ProcessText {
                request_id,
                session_id,
                instructions,
                input,
                output_schema,
                max_output_bytes,
            } => {
                if session_id != self.session_id {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("text processing was requested for another session"),
                    )
                    .map_err(CommandFailure::response);
                }
                // The call is answered inline on the control channel, so anything queued behind
                // it waits — including an `interrupt_turn` that barge-in depends on. A batch
                // call is two provider round trips at worst, which is far longer than the
                // interrupt budget, so it is refused while anything live could need that
                // channel. Post-meeting synthesis, which is what this contract is for, runs
                // when neither is true.
                if voice.active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("text processing cannot run while a voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                }
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("text processing cannot run while a turn is in flight"),
                    )
                    .map_err(CommandFailure::response);
                }
                let request = match TextProcessingRequest::new(
                    instructions,
                    input,
                    output_schema,
                    max_output_bytes,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                // The call runs on the session's own language model, so it runs in the profile
                // the session pinned at start. It creates no turn and touches no conversation
                // state; the guards above are about the control channel it occupies, not about
                // state it could disturb.
                let processed = process_text(
                    self.runtime.language_model(),
                    request,
                    self.provider_failure.clone().unwrap_or_default(),
                )
                .await;
                let processed = match processed {
                    Ok(processed) => client_processed_text(&processed),
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::TextProcessed {
                        request_id,
                        processed,
                    },
                )
                .map_err(CommandFailure::response)
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
                let media_lease = voice_session.media_lease;
                let shutdown = shutdown_voice_pump(voice_session.task)
                    .await
                    .map_err(|error| {
                        CommandFailure::fatal(error, "gateway voice session shutdown failed")
                    });
                revoke_media_lease(media_lease).await;
                shutdown?;
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
    ProviderFailure,
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
            Self::Normal | Self::ProviderFailure | Self::Writer(_) => None,
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
    AlignmentReader(Result<Result<(), RuntimeError>, JoinError>),
    AlignmentItem(Option<RecordingAlignmentItem>),
    Frame(Result<Option<Vec<u8>>, FrameError>),
    ProviderFailure,
    Forwarder(Result<Result<(), GatewaySessionError>, JoinError>),
    VoicePump(Result<Result<(), GatewaySessionError>, JoinError>),
    VoiceControl(Result<Result<(), RuntimeError>, JoinError>),
    VoiceTerminalWritten(Option<()>),
    Writer(Result<Result<(), GatewaySessionError>, JoinError>),
}

async fn shutdown_alignment(active: &mut Option<ActiveAlignmentRead>) {
    if let Some(mut alignment) = active.take() {
        alignment.cancellation.cancel();
        alignment.receiver.close();
        if let Some(task) = alignment.task.take() {
            let _ = task.await;
        }
    }
}

fn command_request_id(command: &ClientCommand) -> &str {
    match command {
        ClientCommand::StartTimedVoiceSession { request_id, .. }
        | ClientCommand::StartDrainableTimedVoiceSession { request_id, .. }
        | ClientCommand::ReadRecordingAlignment { request_id, .. }
        | ClientCommand::RecordingAlignmentCredit { request_id, .. }
        | ClientCommand::CancelRecordingAlignment { request_id, .. }
        | ClientCommand::Status { request_id, .. }
        | ClientCommand::StartTurn { request_id, .. }
        | ClientCommand::InterruptTurn { request_id, .. }
        | ClientCommand::StartVoiceSession { request_id, .. }
        | ClientCommand::StopVoiceSession { request_id, .. }
        | ClientCommand::PauseVoiceCapture { request_id, .. }
        | ClientCommand::ResumeVoiceCapture { request_id, .. }
        | ClientCommand::AuthorizeVoiceResponse { request_id, .. }
        | ClientCommand::SkipVoiceResponse { request_id, .. }
        | ClientCommand::SeedConversationContext { request_id, .. }
        | ClientCommand::MemoryList { request_id, .. }
        | ClientCommand::MemoryInspect { request_id, .. }
        | ClientCommand::PersonaGet { request_id, .. }
        | ClientCommand::PersonaUpdate { request_id, .. }
        | ClientCommand::MemoryApprove { request_id, .. }
        | ClientCommand::MemoryDelete { request_id, .. }
        | ClientCommand::FinalizeRecording { request_id, .. }
        | ClientCommand::ReconcileRecording { request_id, .. }
        | ClientCommand::ReconcileFinalizedRecording { request_id, .. }
        | ClientCommand::ProcessText { request_id, .. }
        | ClientCommand::RenewMediaLease { request_id } => request_id,
    }
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
    terminal_slot: mpsc::OwnedPermit<GatewayMessage>,
    event_writer: mpsc::Sender<GatewayMessage>,
    mut control_events: mpsc::Receiver<VoiceCaptureControlKind>,
    mut control_pending: watch::Receiver<bool>,
    mut notices: VoiceNotices,
) -> Result<(), GatewaySessionError> {
    let mut forwarding = true;
    let mut started = false;
    let mut terminal_slot = Some(terminal_slot);
    let mut exchange = VoiceExchange::default();
    loop {
        // A lost, unresumable media lease is reported once, after the session's first
        // event and before its terminal, as a non-terminal failure: the voice session
        // and its recording stay open until the client stops them.
        let event = tokio::select! {
            biased;
            event = events.recv() => event,
            lost = async { notices.media_lost.as_mut().expect("guarded watch").await },
                if started && notices.media_lost.is_some() =>
            {
                notices.media_lost = None;
                if lost
                    && forwarding
                    && event_writer
                        .send(media_lost_message(notices.session_id))
                        .await
                        .is_err()
                {
                    forwarding = false;
                }
                continue;
            }
            () = async { notices.provider_failure.as_ref().expect("guarded token").cancelled().await },
                if started && notices.provider_failure.is_some() =>
            {
                notices.provider_failure = None;
                if forwarding
                    && event_writer
                        .send(provider_degraded_message(notices.session_id))
                        .await
                        .is_err()
                {
                    forwarding = false;
                }
                continue;
            }
        };
        let Some(event) = event else {
            break;
        };
        started = true;
        if !forwarding {
            continue;
        }
        if let Some(extractor) = extractor.as_ref() {
            match &event {
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TranscriptFinal { turn_id, text },
                    ..
                }
                | VoiceSessionEvent::TranscriptFinal {
                    turn_id: Some(turn_id),
                    text,
                    ..
                } => {
                    exchange.recognized(*turn_id, text);
                }
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TextCompleted { turn_id, text },
                    ..
                } => {
                    if let Some(spoken) = exchange.completed(*turn_id) {
                        extractor.observe_exchange(*turn_id, &spoken, text);
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
            notices.media_lost = None;
            notices.provider_failure = None;
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
        let message = GatewayMessage::VoiceEvent { event };
        if terminal {
            match terminal_slot.take() {
                Some(slot) => {
                    slot.send(message);
                }
                None => forwarding = false,
            }
        } else if event_writer.send(message).await.is_err() {
            forwarding = false;
        }
    }
    Ok(())
}

/// Degradations the pump reports once, between the session's first event and its terminal,
/// without ending the session.
struct VoiceNotices {
    session_id: SessionId,
    media_lost: Option<Pin<Box<dyn Future<Output = bool> + Send>>>,
    provider_failure: Option<CancellationToken>,
}

impl VoiceNotices {
    fn new(
        session_id: SessionId,
        media_lease: Option<&ActiveMediaLease>,
        provider_failure: Option<CancellationToken>,
    ) -> Self {
        Self {
            session_id,
            provider_failure,
            media_lost: media_lease.map(|lease| {
                let server = Arc::clone(&lease.server);
                Box::pin(async move { server.media_lost(session_id).await })
                    as Pin<Box<dyn Future<Output = bool> + Send>>
            }),
        }
    }
}

fn media_lost_message(session_id: SessionId) -> GatewayMessage {
    GatewayMessage::VoiceEvent {
        event: ClientVoiceSessionEvent::VoiceSessionFailed {
            session_id,
            error: ClientRuntimeError {
                code: "media_lease_lost".to_owned(),
                kind: "adapter".to_owned(),
                stage: RuntimeStage::AudioCapture.as_str().to_owned(),
                message: "the media bridge connection was lost and its lease cannot be resumed"
                    .to_owned(),
            },
            recovery: "continue_session".to_owned(),
        },
    }
}

fn provider_degraded_message(session_id: SessionId) -> GatewayMessage {
    GatewayMessage::VoiceEvent {
        event: ClientVoiceSessionEvent::VoiceSessionFailed {
            session_id,
            error: ClientRuntimeError {
                code: "adapter_failure".to_owned(),
                kind: "adapter".to_owned(),
                stage: RuntimeStage::Runtime.as_str().to_owned(),
                message: "a gateway-owned provider process exited; recording continues".to_owned(),
            },
            recovery: "continue_session".to_owned(),
        },
    }
}

// The voice lane splits one exchange across two events: the recognized utterance
// arrives as `TranscriptFinal`, the completed reply as `TextCompleted`. This buffers
// the utterance until its own reply completes.
#[derive(Default)]
struct VoiceExchange {
    spoken: Option<(TurnId, String)>,
}

impl VoiceExchange {
    fn recognized(&mut self, turn_id: TurnId, text: &str) {
        self.spoken = Some((turn_id, text.to_owned()));
    }

    // The utterance this reply answers, or `None` when the buffered one belongs to a
    // different turn. A mismatch means the user spoke again before this reply finished,
    // so the buffered utterance belongs to a turn that has *not* completed yet: it is
    // kept for that turn rather than discarded along with this reply, which would
    // otherwise cost two exchanges instead of one.
    fn completed(&mut self, turn_id: TurnId) -> Option<String> {
        self.spoken
            .take_if(|(spoken_turn, _)| *spoken_turn == turn_id)
            .map(|(_, text)| text)
    }
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
    revoke_media_lease(voice_session.media_lease).await;
}

// A stalled stdout consumer pushes back on stdin instead of failing the session: the next
// command waits up to `COMMAND_REPLY_WAIT` for its replies to fit in the regular lane, then is
// read anyway and its replies use the overflow room. Stop, interrupt, finalize and input EOF
// are therefore reached within that bound; only a client that keeps pipelining past the
// overflow room during a stall reaches writer backpressure. Writer death ends the session
// through its own arm.
async fn read_frame_with_reply_room<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    normal: &mpsc::Sender<GatewayMessage>,
) -> Result<Option<Vec<u8>>, FrameError> {
    // Permits are released at once: only this loop spends unreserved normal-lane slots.
    let room = (COMMAND_REPLY_ROOM + COMMAND_OVERFLOW_ROOM).min(normal.max_capacity());
    let _ = timeout(COMMAND_REPLY_WAIT, normal.reserve_many(room)).await;
    reader.read_frame().await
}

async fn revoke_media_lease(lease: Option<ActiveMediaLease>) {
    if let Some(lease) = lease {
        lease.server.revoke(lease.session_id).await;
    }
}

async fn writer_loop<W>(
    writer: W,
    mut urgent: mpsc::Receiver<GatewayMessage>,
    mut normal: mpsc::Receiver<GatewayMessage>,
    mut events: mpsc::Receiver<GatewayMessage>,
    terminal_written: mpsc::UnboundedSender<()>,
    negotiated_version: Arc<AtomicU64>,
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
                write_gateway_message(
                    &mut writer,
                    queued,
                    &terminal_written,
                    negotiated_version.load(Ordering::Relaxed),
                )
                .await?;
            }
        }
        write_gateway_message(
            &mut writer,
            message,
            &terminal_written,
            negotiated_version.load(Ordering::Relaxed),
        )
        .await?;
    }
    Ok(())
}

/// The lowest protocol version this session's status can be stated at.
///
/// A gateway whose status names a capability an older version does not define
/// cannot describe itself to that client truthfully, so it keeps answering at
/// the lowest version its own status encodes cleanly.
fn minimum_status_version(status: &RuntimeStatus) -> u64 {
    let ready = GatewayMessage::Ready {
        status: status.clone(),
    };
    [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    ]
    .into_iter()
    .find(|version| encode_gateway_message_for_version(&ready, *version).is_ok())
    .unwrap_or(DEFAULT_CLIENT_PROTOCOL_VERSION)
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
    protocol_version: u64,
) -> Result<(), GatewaySessionError>
where
    W: AsyncWrite + Unpin,
{
    let terminal = is_terminal_voice_message(&message);
    let payload = encode_gateway_message_for_version(&message, protocol_version)
        .map_err(|_| GatewaySessionError::Encoding)?;
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
        media_lease: None,
    }
}

fn accepted_turn_message(request_id: &str, turn_id: TurnId) -> GatewayMessage {
    GatewayMessage::CommandAccepted {
        request_id: request_id.to_owned(),
        turn_id: Some(turn_id),
        media_lease: None,
    }
}

fn media_lease_descriptor(
    server: &MediaServer,
    lease: &MediaLease,
) -> Result<ClientMediaLeaseDescriptor, ()> {
    let reconnect_window_ms =
        u64::try_from(lease.reconnect_window().as_millis()).map_err(|_| ())?;
    ClientMediaLeaseDescriptor::new(
        MEDIA_PROTOCOL_VERSION,
        lease.session_id(),
        server.advertised_endpoint().to_owned(),
        lease.credential().to_owned(),
        lease.expires_at_ms(),
        reconnect_window_ms,
    )
    .map_err(|_| ())
}

fn send_normal(
    writer: &mpsc::Sender<GatewayMessage>,
    message: GatewayMessage,
) -> Result<(), GatewaySessionError> {
    send_bounded(writer, message)
}

fn send_reconciliation(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: String,
    reconciliation: ClientRecordingReconciliation,
    protocol_version: u64,
) -> Result<(), GatewaySessionError> {
    let message = GatewayMessage::RecordingReconciled {
        request_id: request_id.clone(),
        reconciliation,
    };
    // Includes request ID, JSON escaping and every populated v4 evidence object. Validate
    // the complete serialized frame before accepting; never truncate a successful answer.
    if encode_gateway_message_for_version(&message, protocol_version).is_err() {
        return send_rejection(
            writer,
            &request_id,
            command_error("recording reconciliation exceeded its wire contract"),
        );
    }
    send_accepted(writer, &request_id)?;
    send_normal(writer, message)
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

fn provider_failure_message() -> GatewayMessage {
    GatewayMessage::Fatal {
        error: ClientRuntimeError {
            code: "adapter_failure".to_owned(),
            kind: "adapter".to_owned(),
            stage: "runtime".to_owned(),
            message: "a gateway-owned provider process exited".to_owned(),
        },
    }
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

/// Projects a finalized recording onto the wire. The path comes from the TOML configuration
/// and is therefore already UTF-8; the lossy conversion is the unreachable branch, not the
/// intended one. The key appears in neither form — only `key_id`, a digest of it.
fn client_recording_summary(summary: &RecordingSummary) -> ClientRecordingSummary {
    ClientRecordingSummary {
        recording_handle: summary.recording_handle.clone(),
        encrypted_path: summary.encrypted_path.to_string_lossy().into_owned(),
        sha256: summary.sha256.clone(),
        duration_ms: summary.duration_ms,
        format: summary.format.clone(),
        key_id: summary.key_id.clone(),
        gaps: summary
            .gaps
            .iter()
            .map(|gap| ClientRecordingGap {
                started_at_ms: gap.started_at_ms,
                ended_at_ms: gap.ended_at_ms,
                reason: gap.reason.clone(),
            })
            .collect(),
    }
}

/// Projects one processed answer onto the wire. The output is already canonical JSON the
/// runtime validated against the caller's schema, so this only renames the warnings.
fn client_processed_text(processed: &ProcessedText) -> ClientProcessedText {
    ClientProcessedText {
        output: processed.output().to_owned(),
        warnings: processed
            .warnings()
            .iter()
            .map(|warning| warning.as_str().to_owned())
            .collect(),
    }
}

/// Projects a reconciliation onto the wire. Cluster labels arrive anonymous from the adapter
/// and are carried through unchanged; the wire refuses anything that is not one, so a provider
/// identity cannot reach a client even by a route this function does not know about.
fn client_reconciliation(result: &DiarizationResult) -> ClientRecordingReconciliation {
    ClientRecordingReconciliation {
        segments: result
            .segments()
            .iter()
            .map(|segment| ClientDiarizedSegment {
                segment_id: segment.segment_id().to_owned(),
                started_at_ms: segment.started_at_ms(),
                ended_at_ms: segment.ended_at_ms(),
                text: segment.text().to_owned(),
                speaker_cluster: segment.speaker_cluster().map(str::to_owned),
                media_sample_range: segment.media_sample_range(),
            })
            .collect(),
        warnings: result
            .warnings()
            .iter()
            .map(|warning| warning.as_str().to_owned())
            .collect(),
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
        AdapterError, AdapterFuture, AudioFrame, CaptureEvent, DiarizationResult, DiarizedSegment,
        GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
        MeetingAudioWindows, MockContinuousAudioOutput, MockGenerationLanguageModel,
        MockStreamingSpeechSynthesizer, MockVoiceCaptureControl, MockVoiceIoFactory, OllamaConfig,
        OllamaLanguageModel, OpenAiSpeechRecognitionConfig, PcmFormat, PcmSampleFormat,
        RecognitionEvent, RecognitionHypothesis, StreamingSpeechRequest,
        StreamingSpeechSynthesizer, TranscriptionDiarizer, VoiceCaptureControl, VoiceInput,
        VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
    };
    use conversation_protocol::{
        ClientRuntimeEvent, ComponentDescriptor, ComponentKind, ExecutionLocation, GatewayMessage,
        GenerationId, MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance,
        MemoryProvenanceKind, MemoryRetention, PrivacyMode, RecoveryDisposition, RuntimeError,
        RuntimeErrorKind, RuntimeStage, RuntimeStatus, SessionId, TurnId, UnixTimestampMillis,
        UtteranceId, VoiceActivity, CLIENT_PROTOCOL_VERSION, MAX_CLIENT_FRAME_BYTES,
        MAX_MEMORY_CONTENT_BYTES,
    };
    use tempfile::TempDir;
    use tokio::io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, Mutex, Notify};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use conversation_runtime::{
        read_session_recording, resolve_recording_handle, RecordedChunk, RecordingSettings,
    };

    use crate::voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};
    use crate::{FrameReader, MediaServer, MediaServerConfig};

    use super::{voice_failure_diagnostic, GatewaySession, GatewaySessionError, VoiceExchange};

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

    /// Catches the production break where a v3 media lease is accepted before its
    /// session-specific network voice I/O is attached, where a bridge frame reaches only one
    /// capture consumer, or where the stop path leaves that credential usable by a bridge.
    #[tokio::test]
    async fn media_v3_start_correlates_a_live_lease_and_stop_revokes_its_bridge_token() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_MEDIA_FANOUT";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let (voice, _guards) = test_voice_adapters();
        let session_id = SessionId::new(701);
        let (recognition_handoff, mut recognition_handoffs) = mpsc::channel(1);
        let session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(voice, test_language())
            .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
            .with_media_server(Arc::clone(&media))
            .with_media_capture_handoff(recognition_handoff)
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"media-status"}"#)
            .await;
        let status_accepted = gateway.read_message().await;
        assert_accepted_message(&status_accepted, "media-status");
        assert!(!status_accepted.contains("media_lease"));
        let _ = gateway.read_message().await;

        gateway
            .write(
                r#"{"protocol_version":3,"type":"start_voice_session","request_id":"media-start"}"#,
            )
            .await;
        let accepted = gateway.read_message().await;
        let accepted: serde_json::Value = serde_json::from_str(&accepted).unwrap();
        let lease = accepted
            .get("media_lease")
            .expect("v3 media start did not return a correlated lease");
        assert_eq!(lease["session_id"], "701");
        assert_eq!(lease["connect_endpoint"], media.advertised_endpoint());
        let credential = lease["credential"].as_str().unwrap();
        let endpoint: std::net::SocketAddr = lease["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();

        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        let recognition = timeout(TEST_TIMEOUT, recognition_handoffs.recv())
            .await
            .expect("gateway did not hand off its production recognition capture")
            .expect("gateway dropped its production recognition capture");
        let mut recognition = recognition
            .start(session_id, CancellationToken::new())
            .await
            .unwrap();

        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, credential))
            .await
            .unwrap();
        bridge
            .write_all(&media_capture(session_id, 1, 1_000, b"live"))
            .await
            .unwrap();
        let capture = timeout(TEST_TIMEOUT, recognition.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match capture {
            CaptureEvent::Frame(frame) => {
                assert_eq!(frame.sequence(), 1);
                assert_eq!(frame.bytes(), b"live");
            }
            event => panic!("expected capture frame, received {event:?}"),
        }
        let encrypted_path = wait_for_recording_bytes(directory.path(), 80).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"media-second"}"#)
            .await;
        let second = gateway
            .read_until(|message| message.contains(r#""request_id":"media-second""#))
            .await;
        let rejection = second
            .last()
            .expect("read_until returns the frame it matched");
        assert_rejected_message(rejection, "media-second", "invalid_state");
        assert!(!rejection.contains("media_lease"));

        gateway
            .write(
                r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"media-stop"}"#,
            )
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"media-stop""#)
            })
            .await;
        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"media-finalize","session_id":"701"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "media-finalize");
        let finalized: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let handle = finalized["recording"]["recording_handle"]
            .as_str()
            .expect("recording finalization did not publish a handle");
        assert_eq!(
            resolve_recording_handle(directory.path(), handle).unwrap(),
            encrypted_path
        );
        let decrypted = read_session_recording(&encrypted_path, key_env).unwrap();
        assert_eq!(decrypted.session_id, session_id);
        assert!(!decrypted.truncated);
        let recorded = decrypted
            .chunks
            .iter()
            .filter_map(|chunk| match chunk {
                RecordedChunk::Frame {
                    sequence, bytes, ..
                } => Some((*sequence, bytes.as_slice())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recorded, vec![(1, b"live".as_slice())]);
        let mut byte = [0_u8; 1];
        assert_eq!(bridge.read(&mut byte).await.unwrap(), 0);

        let mut replay = TcpStream::connect(endpoint).await.unwrap();
        replay
            .write_all(&media_authentication(session_id, credential))
            .await
            .unwrap();
        assert_eq!(replay.read(&mut byte).await.unwrap(), 0);

        gateway.close().await;
        media.shutdown().await;
    }

    async fn media_lease_gateway(
        directory: &TempDir,
        key_env: &str,
        session_id: SessionId,
        credential_lifetime: Duration,
        provider_failure: Option<CancellationToken>,
    ) -> (
        Arc<MediaServer>,
        InMemoryGateway,
        String,
        std::net::SocketAddr,
        mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
    ) {
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let media = Arc::new(
            MediaServer::bind(
                MediaServerConfig::loopback()
                    .with_lifetimes(credential_lifetime, Duration::from_secs(5)),
            )
            .await
            .unwrap(),
        );
        let (voice, _guards) = test_voice_adapters();
        let (recognition_handoff, mut recognition_handoffs) = mpsc::channel(1);
        let mut session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(voice, test_language())
            .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
            .with_media_server(Arc::clone(&media))
            .with_media_capture_handoff(recognition_handoff)
            .with_test_session_id(session_id);
        if let Some(provider_failure) = provider_failure {
            session = session.with_provider_failure(provider_failure);
        }
        let mut gateway = InMemoryGateway::start(session).await;
        gateway
            .write(
                r#"{"protocol_version":3,"type":"start_voice_session","request_id":"lease-start"}"#,
            )
            .await;
        let accepted: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let lease = &accepted["media_lease"];
        let credential = lease["credential"].as_str().unwrap().to_owned();
        let endpoint = lease["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        let recognition = timeout(TEST_TIMEOUT, recognition_handoffs.recv())
            .await
            .unwrap()
            .unwrap()
            .start(session_id, CancellationToken::new())
            .await
            .unwrap();
        (media, gateway, credential, endpoint, recognition)
    }

    async fn authenticated_bridge(
        endpoint: std::net::SocketAddr,
        session_id: SessionId,
        credential: &str,
    ) -> TcpStream {
        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, credential))
            .await
            .unwrap();
        bridge
    }

    async fn bridge_refused(
        endpoint: std::net::SocketAddr,
        session_id: SessionId,
        credential: &str,
    ) -> bool {
        let mut bridge = authenticated_bridge(endpoint, session_id, credential).await;
        let mut byte = [0_u8; 1];
        matches!(
            timeout(TEST_TIMEOUT, bridge.read(&mut byte)).await,
            Ok(Ok(0) | Err(_))
        )
    }

    /// Connects a legacy bridge and sends one frame, retrying while an earlier connection
    /// still holds the admission fence. Admission is proven by the frame reaching the
    /// session's capture fan-out, which records before it offers recognition.
    async fn admitted_bridge(
        endpoint: std::net::SocketAddr,
        session_id: SessionId,
        credential: &str,
        recognition: &mut mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
        sequence: u64,
        payload: &[u8],
    ) -> TcpStream {
        timeout(TEST_TIMEOUT, async {
            loop {
                let mut bridge = authenticated_bridge(endpoint, session_id, credential).await;
                bridge
                    .write_all(&media_capture(
                        session_id,
                        sequence,
                        sequence * 1_000,
                        payload,
                    ))
                    .await
                    .unwrap();
                let mut byte = [0_u8; 1];
                loop {
                    tokio::select! {
                        _ = bridge.read(&mut byte) => break,
                        event = recognition.recv() => {
                            if let Some(Ok(CaptureEvent::Frame(frame))) = event {
                                if frame.sequence() == sequence {
                                    return bridge;
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the bridge was never admitted")
    }

    fn recorded_frames(path: &std::path::Path, key_env: &str) -> Vec<(u64, Vec<u8>)> {
        read_session_recording(path, key_env)
            .unwrap()
            .chunks
            .iter()
            .filter_map(|chunk| match chunk {
                RecordedChunk::Frame {
                    sequence, bytes, ..
                } => Some((*sequence, bytes.clone())),
                _ => None,
            })
            .collect()
    }

    /// A bridge drop late in a meeting must stay resumable while the control plane keeps
    /// the lease renewed. The credential is unchanged, only its expiry moves, and the bridge
    /// reconnects with it after the original lifetime so the recording continues.
    #[tokio::test]
    async fn a_renewed_media_lease_lets_a_dropped_bridge_resume_after_its_original_lifetime() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_MEDIA_RENEWAL";
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new(702);
        let (media, mut gateway, credential, endpoint, mut recognition) = media_lease_gateway(
            &directory,
            key_env,
            session_id,
            Duration::from_millis(1_500),
            None,
        )
        .await;
        let bridge = admitted_bridge(
            endpoint,
            session_id,
            &credential,
            &mut recognition,
            1,
            b"before",
        )
        .await;

        let mut expires_at_ms = 0_u64;
        for round in 0..4 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let request = format!("renew-{round}");
            gateway
                .write(&format!(
                    r#"{{"protocol_version":3,"type":"renew_media_lease","request_id":"{request}"}}"#
                ))
                .await;
            let needle = format!(r#""request_id":"{request}""#);
            let reply = gateway
                .read_until(|message| {
                    message.contains(&needle) && message_type(message) != "command_accepted"
                })
                .await
                .pop()
                .unwrap();
            let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(reply["type"], "media_lease_renewed", "{reply}");
            let renewed: u64 = reply["expires_at_ms"].as_str().unwrap().parse().unwrap();
            assert!(renewed > expires_at_ms);
            expires_at_ms = renewed;
        }

        // Two seconds past admission: the original 1.5 s credential has lapsed.
        drop(bridge);
        let _bridge = admitted_bridge(
            endpoint,
            session_id,
            &credential,
            &mut recognition,
            2,
            b"resume",
        )
        .await;

        gateway
            .write(
                r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"lease-stop"}"#,
            )
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"lease-stop""#)
            })
            .await;
        // The lease ends with the session: neither renewal nor reconnect revives it.
        gateway
            .write(r#"{"protocol_version":3,"type":"renew_media_lease","request_id":"renew-after-stop"}"#)
            .await;
        assert_rejected_message(
            &gateway.read_message().await,
            "renew-after-stop",
            "invalid_state",
        );
        assert!(bridge_refused(endpoint, session_id, &credential).await);
        let frames = recorded_frames(
            &wait_for_recording_bytes(directory.path(), 1).await,
            key_env,
        );
        assert!(frames.contains(&(1, b"before".to_vec())), "{frames:?}");
        assert!(frames.contains(&(2, b"resume".to_vec())), "{frames:?}");
        gateway.close().await;
        media.shutdown().await;
    }

    /// When a dropped bridge can no longer reconnect, the client gets one explicit,
    /// non-terminal audio-capture failure so it can record the gap and act. The session and
    /// its recording stay open until the client stops them, and a lapsed lease is not renewed.
    #[tokio::test]
    async fn an_unresumable_media_drop_is_reported_without_ending_the_recording() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_MEDIA_LOSS";
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new(703);
        let (media, mut gateway, credential, endpoint, mut recognition) = media_lease_gateway(
            &directory,
            key_env,
            session_id,
            Duration::from_millis(300),
            None,
        )
        .await;
        let bridge = admitted_bridge(
            endpoint,
            session_id,
            &credential,
            &mut recognition,
            1,
            b"kept",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(bridge);

        let messages = gateway
            .read_until(|message| {
                message.contains(r#""type":"voice_session_failed""#)
                    && message.contains(r#""stage":"audio_capture""#)
            })
            .await;
        let failure: serde_json::Value = serde_json::from_str(messages.last().unwrap()).unwrap();
        assert_eq!(failure["event"]["recovery"], "continue_session");
        assert_eq!(failure["event"]["session_id"], "703");
        assert_eq!(failure["event"]["error"]["code"], "media_lease_lost");
        assert!(messages.iter().all(|message| !is_voice_terminal(message)));
        assert!(bridge_refused(endpoint, session_id, &credential).await);

        gateway
            .write(
                r#"{"protocol_version":3,"type":"renew_media_lease","request_id":"renew-lapsed"}"#,
            )
            .await;
        let reply = gateway
            .read_until(|message| message.contains(r#""request_id":"renew-lapsed""#))
            .await
            .pop()
            .unwrap();
        assert_rejected_message(&reply, "renew-lapsed", "invalid_state");

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"loss-stop"}"#)
            .await;
        let stopped = gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"loss-stop""#)
            })
            .await;
        assert!(stopped.iter().any(|message| is_voice_terminal(message)));
        assert!(stopped
            .iter()
            .all(|message| !message.contains(r#""stage":"audio_capture""#)));
        gateway
            .write(r#"{"protocol_version":3,"type":"finalize_recording","request_id":"loss-finalize","session_id":"703"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "loss-finalize");
        let finalized: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        assert_eq!(finalized["type"], "recording_finalized");
        assert_eq!(
            recorded_frames(
                &wait_for_recording_bytes(directory.path(), 1).await,
                key_env
            ),
            vec![(1, b"kept".to_vec())]
        );
        gateway.close().await;
        media.shutdown().await;
    }

    /// A supervised provider-host exit degrades recognition and synthesis only. The active
    /// voice session keeps recording, the client is told without a terminal, new work is
    /// refused, and the gateway session ends with the provider failure only once the recording
    /// has been stopped and finalized.
    #[tokio::test]
    async fn a_provider_host_exit_keeps_the_active_recording_until_it_is_finalized() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_PROVIDER_EXIT";
        let directory = TempDir::new().unwrap();
        let session_id = SessionId::new(704);
        let provider_failure = CancellationToken::new();
        let (media, mut gateway, credential, endpoint, mut recognition) = media_lease_gateway(
            &directory,
            key_env,
            session_id,
            Duration::from_secs(60),
            Some(provider_failure.clone()),
        )
        .await;
        let mut bridge = admitted_bridge(
            endpoint,
            session_id,
            &credential,
            &mut recognition,
            1,
            b"before",
        )
        .await;

        provider_failure.cancel();
        let messages = gateway
            .read_until(|message| {
                message_type(message) == "fatal"
                    || (message.contains(r#""type":"voice_session_failed""#)
                        && message.contains(r#""stage":"runtime""#))
            })
            .await;
        let failure: serde_json::Value = serde_json::from_str(messages.last().unwrap()).unwrap();
        assert_eq!(failure["type"], "voice_event", "{failure}");
        assert_eq!(failure["event"]["recovery"], "continue_session");
        assert!(messages.iter().all(|message| !is_voice_terminal(message)));

        bridge
            .write_all(&media_capture(session_id, 2, 2_000, b"during"))
            .await
            .unwrap();
        timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(Ok(CaptureEvent::Frame(frame))) = recognition.recv().await {
                    if frame.sequence() == 2 {
                        return;
                    }
                }
            }
        })
        .await
        .expect("capture stopped after the provider host exited");

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"degraded-stop"}"#)
            .await;
        let stopped = gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"degraded-stop""#)
            })
            .await;
        assert!(stopped.iter().any(|message| is_voice_terminal(message)));
        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"degraded-start"}"#)
            .await;
        let refused = gateway.read_message().await;
        assert_rejected_message(&refused, "degraded-start", "invalid_state");
        assert!(refused.contains("provider process exited"), "{refused}");
        gateway
            .write(r#"{"protocol_version":3,"type":"finalize_recording","request_id":"degraded-finalize","session_id":"704"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "degraded-finalize");
        let finalized: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        assert_eq!(finalized["type"], "recording_finalized");
        let fatal = gateway.read_message().await;
        assert_eq!(message_type(&fatal), "fatal");
        assert!(fatal.contains(r#""code":"adapter_failure""#), "{fatal}");
        timeout(TEST_TIMEOUT, &mut gateway.task)
            .await
            .expect("the degraded gateway session did not end after finalization")
            .unwrap()
            .unwrap();
        let frames = recorded_frames(
            &wait_for_recording_bytes(directory.path(), 1).await,
            key_env,
        );
        assert!(frames.contains(&(1, b"before".to_vec())), "{frames:?}");
        assert!(frames.contains(&(2, b"during".to_vec())), "{frames:?}");
        media.shutdown().await;
    }

    /// Test writer whose output can be stalled and resumed at any point.
    #[derive(Clone, Default)]
    struct GatedWriter {
        bytes: Arc<StdMutex<Vec<u8>>>,
        stalled: Arc<AtomicBool>,
        waker: Arc<StdMutex<Option<Waker>>>,
    }

    impl GatedWriter {
        fn set_stalled(&self, stalled: bool) {
            self.stalled.store(stalled, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes.lock().unwrap()).into_owned()
        }

        async fn wait_for(&self, expected: &str) {
            timeout(TEST_TIMEOUT, async {
                while !self.text().contains(expected) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("gateway never wrote {expected}"));
        }
    }

    impl AsyncWrite for GatedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.stalled.load(Ordering::SeqCst) {
                *self.waker.lock().unwrap() = Some(context.waker().clone());
                return Poll::Pending;
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A renewal reply that waits behind a stalled consumer for longer than one lifetime
    /// is still written late; it never fails the writer or ends the session.
    #[tokio::test]
    async fn a_renewal_reply_delayed_past_its_lifetime_is_written_without_ending_the_session() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_LATE_RENEWAL";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let media = Arc::new(
            MediaServer::bind(
                MediaServerConfig::loopback()
                    .with_lifetimes(Duration::from_millis(400), Duration::from_millis(400)),
            )
            .await
            .unwrap(),
        );
        let (voice, _guards) = test_voice_adapters();
        let session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(voice, test_language())
            .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(SessionId::new(705));
        let (mut input, reader) = duplex(4096);
        let writer = GatedWriter::default();
        let session_task = tokio::spawn(session.run(reader, writer.clone()));
        write_command(
            &mut input,
            r#"{"protocol_version":3,"type":"start_voice_session","request_id":"late-start"}"#,
        )
        .await;
        writer.wait_for(r#""type":"voice_session_started""#).await;

        writer.set_stalled(true);
        write_command(
            &mut input,
            r#"{"protocol_version":3,"type":"renew_media_lease","request_id":"late-renew"}"#,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        writer.set_stalled(false);
        writer.wait_for(r#""type":"media_lease_renewed""#).await;
        assert!(!session_task.is_finished());

        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not end at input EOF")
            .unwrap()
            .unwrap();
        media.shutdown().await;
    }

    /// Catches a Task 7 regression where the online recognizer is built against a fresh capture
    /// (or an offline sidecar) instead of the exact Task 6 capture already attached to this
    /// session's network factory. The bridge frame must reach the scripted recognizer while the
    /// externally-gated session remains able to skip it without generating a response.
    #[tokio::test]
    async fn online_media_capture_reaches_the_selected_recognizer_and_skip_keeps_the_bridge_alive()
    {
        const ONLINE_CREDENTIAL: &str = "CONVERSATION_ONLINE_MEDIA_TEST_CREDENTIAL";
        const RECORDING_KEY: &str = "CONVERSATION_ONLINE_MEDIA_RECORDING_KEY";
        std::env::set_var(ONLINE_CREDENTIAL, "fake-not-a-real-credential-0000");
        std::env::set_var(RECORDING_KEY, FAKE_RECORDING_KEY);
        let recognizer = ScriptedRecognizerServer::start().await;
        let directory = TempDir::new().unwrap();
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(705);
        let recognition = OpenAiSpeechRecognitionConfig::new(
            recognizer.endpoint(),
            "scripted-asr-model",
            ONLINE_CREDENTIAL,
        )
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let speech = Arc::new(PerRequestSpeech::default());
        let voice = GatewayVoiceAdapters {
            io: GatewayVoiceAdapters::unavailable_online_io(),
            speech: speech.clone(),
            policy: voice_policy(),
            online_recognition: Some(recognition),
        };
        let session = GatewaySession::new(unused_runtime(), online_status_with_voice())
            .with_voice(voice, language.clone())
            .with_recording(RecordingSettings::new(directory.path(), RECORDING_KEY).unwrap())
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"online-start","response_mode":"external_gate"}"#)
            .await;
        let accepted: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let lease = accepted.get("media_lease").unwrap();
        let endpoint: std::net::SocketAddr = lease["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["credential"].as_str().unwrap().to_owned();
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;

        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(session_id, 1, 1_000, b"online"))
            .await
            .unwrap();
        recognizer.wait_for_request().await;

        let final_transcript = gateway
            .read_until(|message| message.contains(r#""type":"voice_transcript_final""#))
            .await;
        assert!(final_transcript
            .iter()
            .any(|message| message.contains(r#""text":"agenda item""#)));
        tokio::time::sleep(Duration::from_millis(850)).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"skip_voice_response","request_id":"online-skip","session_id":"705","segment_id":"1","reason":"meeting policy"}"#)
            .await;
        let skipped = gateway
            .read_until(|message| message.contains(r#""type":"response_skipped""#))
            .await;
        assert!(skipped
            .iter()
            .any(|message| message.contains("meeting policy")));
        assert!(skipped.iter().any(|message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"online-skip""#)
        }));
        assert!(
            language.requests().is_empty(),
            "skip started text generation"
        );
        assert!(
            speech.requests().is_empty(),
            "skip started speech synthesis"
        );

        let encrypted_path = wait_for_recording_bytes(directory.path(), 80).await;
        gateway
            .write(
                r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"online-stop"}"#,
            )
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"online-stop""#)
            })
            .await;
        gateway
            .write(r#"{"protocol_version":3,"type":"finalize_recording","request_id":"online-finalize","session_id":"705"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "online-finalize");
        let finalized: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let handle = finalized["recording"]["recording_handle"].as_str().unwrap();
        assert_eq!(
            resolve_recording_handle(directory.path(), handle).unwrap(),
            encrypted_path
        );
        let recording = read_session_recording(&encrypted_path, RECORDING_KEY).unwrap();
        assert!(recording.chunks.iter().any(|chunk| {
            matches!(chunk, RecordedChunk::Frame { sequence: 1, bytes, .. } if bytes == b"online")
        }));

        gateway.close().await;
        recognizer.finish().await;
        media.shutdown().await;
    }

    /// The online bridge must also enter the existing external authorization flow: a scripted
    /// final transcript is inert until the meeting host explicitly authorizes that segment.
    #[tokio::test]
    async fn online_media_recognizer_authorizes_only_the_host_approved_response() {
        const ONLINE_CREDENTIAL: &str = "CONVERSATION_ONLINE_AUTHORIZE_TEST_CREDENTIAL";
        std::env::set_var(ONLINE_CREDENTIAL, "fake-not-a-real-credential-0000");
        let recognizer = ScriptedRecognizerServer::start().await;
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(706);
        let recognition = OpenAiSpeechRecognitionConfig::new(
            recognizer.endpoint(),
            "scripted-asr-model",
            ONLINE_CREDENTIAL,
        )
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
        let speech = Arc::new(PerRequestSpeech::default());
        let voice = GatewayVoiceAdapters {
            io: GatewayVoiceAdapters::unavailable_online_io(),
            speech: speech.clone(),
            policy: voice_policy(),
            online_recognition: Some(recognition),
        };
        let language = Arc::new(MockGenerationLanguageModel::new(["approved reply"]));
        let session = GatewaySession::new(unused_runtime(), online_status_with_voice())
            .with_voice(voice, language.clone())
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"authorize-start","response_mode":"external_gate"}"#)
            .await;
        let lease: serde_json::Value = serde_json::from_str(&gateway.read_message().await).unwrap();
        let endpoint: std::net::SocketAddr = lease["media_lease"]["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["media_lease"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(session_id, 1, 1_000, b"online"))
            .await
            .unwrap();
        recognizer.wait_for_request().await;
        gateway
            .read_until(|message| message.contains(r#""type":"voice_transcript_final""#))
            .await;
        tokio::time::sleep(Duration::from_millis(850)).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"authorize_voice_response","request_id":"online-authorize","session_id":"706","segment_id":"1","context_exchanges":[{"user":"prior","assistant":"context"}]}"#)
            .await;
        let authorized = gateway
            .read_until(|message| message.contains(r#""type":"response_authorized""#))
            .await;
        assert!(authorized.iter().any(|message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"online-authorize""#)
        }));
        assert!(authorized
            .iter()
            .any(|message| message.contains(r#""type":"response_authorized""#)));
        timeout(TEST_TIMEOUT, async {
            loop {
                if language.requests().len() == 1 && speech.requests().len() == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authorization did not reach text generation and speech synthesis");
        assert_eq!(language.requests()[0].transcript(), "agenda item");
        assert_eq!(speech.requests()[0].text(), "approved reply");

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"authorize-stop"}"#)
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"authorize-stop""#)
            })
            .await;
        gateway.close().await;
        recognizer.finish().await;
        media.shutdown().await;
    }

    /// Catches a Task 7 regression where an externally gated online meeting reaches only a
    /// mock language or speech boundary after the real recognizer finalizes a bridge capture.
    /// Production ASR must traverse WebSocket and language/speech their HTTP contracts,
    /// and the returned streaming WAV must become a playable media-bridge frame.
    #[tokio::test]
    async fn online_external_gate_runs_production_asr_language_and_streaming_speech_over_loopback()
    {
        const CREDENTIAL: &str = "CONVERSATION_ONLINE_CHAIN_CREDENTIAL";
        std::env::set_var(CREDENTIAL, "fake-not-a-real-credential-0000");
        let recognizer = ScriptedRecognizerServer::two_chunks().await;
        let language = ScriptedHttpProvider::start(language_sse_response("approved reply.")).await;
        let speech_pcm = pcm16_wav(16_000, 1, 160, 0x1234);
        let speech = ScriptedHttpProvider::start(http_response(&speech_pcm)).await;
        let recognition_audio = vec![b'o'; 32_000];
        let session_id = SessionId::new(709);
        let mut adapters = loader_built_online_adapters(
            recognizer.endpoint(),
            language.endpoint(),
            speech.endpoint(),
            CREDENTIAL,
        );
        let media = Arc::new(
            MediaServer::bind(adapters.media.take().unwrap())
                .await
                .unwrap(),
        );
        let runtime = conversation_runtime::TextTurnRuntime::new(
            adapters.context,
            Arc::clone(&adapters.language),
        );
        let session = GatewaySession::new(runtime, adapters.status)
            .with_voice(adapters.voice.take().unwrap(), adapters.language)
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"chain-start","response_mode":"external_gate"}"#)
            .await;
        let lease: serde_json::Value = serde_json::from_str(&gateway.read_message().await).unwrap();
        let endpoint: std::net::SocketAddr = lease["media_lease"]["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["media_lease"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(
                session_id,
                1,
                1_000,
                &recognition_audio[..16_000],
            ))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(
                session_id,
                2,
                1_001,
                &recognition_audio[16_000..],
            ))
            .await
            .unwrap();

        recognizer.wait_for_request().await;
        let final_transcript = gateway
            .read_until(|message| message.contains(r#""type":"voice_transcript_final""#))
            .await;
        assert!(final_transcript
            .iter()
            .any(|message| message.contains(r#""text":"agenda item""#)));
        assert_eq!(recognizer.request_count().await, 2);
        assert_eq!(language.request_count().await, 0);
        assert_eq!(speech.request_count().await, 0);

        tokio::time::sleep(Duration::from_millis(850)).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"authorize_voice_response","request_id":"chain-authorize","session_id":"709","segment_id":"1","context_exchanges":[{"user":"prior","assistant":"context"}]}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""type":"response_authorized""#))
            .await;
        language.wait_for_request().await;
        speech.wait_for_request().await;

        let recognition_request = recognizer.request(0).await;
        assert_eq!(
            recognition_request.target,
            "/v1/realtime?intent=transcription"
        );
        assert_eq!(
            recognition_request.authorization.as_deref(),
            Some("Bearer fake-not-a-real-credential-0000")
        );
        let mut recognition_keys = recognition_request
            .payload
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        recognition_keys.sort();
        assert_eq!(recognition_keys, ["audio", "type"]);
        assert_eq!(
            recognition_request.payload["type"],
            "input_audio_buffer.append"
        );
        // 16,000 literal bytes 0x6f = 5,333 complete b29v groups and one bw== tail.
        let expected_audio = format!("{}bw==", "b29v".repeat(5_333));
        assert_eq!(recognition_request.payload["audio"], expected_audio);
        assert_eq!(
            recognizer.request(1).await.payload,
            recognition_request.payload
        );
        let update = recognizer.update().await;
        assert_eq!(
            update["session"]["audio"]["input"]["transcription"]["model"],
            "scripted-asr-model"
        );
        assert_eq!(
            update["session"]["audio"]["input"]["turn_detection"],
            serde_json::json!({
                "type":"server_vad", "threshold":0.5, "prefix_padding_ms":300, "silence_duration_ms":500
            })
        );
        let language_request = language.request(0).await;
        assert_eq!(language_request.target, "/v1/chat/completions");
        assert_eq!(
            language_request.authorization.as_deref(),
            Some("Bearer fake-not-a-real-credential-0000")
        );
        assert_eq!(language_request.payload["model"], "scripted-language-model");
        assert_eq!(language_request.payload["stream"], true);
        let messages = language_request.payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        let context_start = messages.len() - 3;
        assert_eq!(messages[context_start]["role"], "user");
        assert_eq!(messages[context_start]["content"], "prior");
        assert_eq!(messages[context_start + 1]["role"], "assistant");
        assert_eq!(messages[context_start + 1]["content"], "context");
        assert_eq!(messages[context_start + 2]["role"], "user");
        assert_eq!(messages[context_start + 2]["content"], "agenda item");
        let speech_request = speech.request(0).await;
        assert_eq!(speech_request.target, "/v1/audio/speech");
        assert_eq!(
            speech_request.authorization.as_deref(),
            Some("Bearer fake-not-a-real-credential-0000")
        );
        assert_eq!(
            speech_request.payload,
            serde_json::json!({
                "model": "scripted-speech-model",
                "input": "approved reply.",
                "voice": "scripted-voice",
                "speed": 1.0,
                "lang_code": "auto",
                "instruct": "Speak clearly.",
                "max_tokens": 128,
                "repetition_penalty": 1.0,
                "response_format": "wav",
                "stream": true,
                "streaming_interval": 0.2,
            })
        );

        let playback_sequence = read_loopback_playback(&mut bridge, session_id, &speech_pcm).await;
        send_loopback_playback_ack(&mut bridge, session_id, playback_sequence).await;
        let accepted_playback = gateway
            .read_until(|message| {
                message.contains(r#""type":"voice_playback""#)
                    && message.contains(r#""state":"accepted""#)
            })
            .await;
        assert!(accepted_playback.iter().any(|message| {
            message.contains(r#""type":"voice_playback""#)
                && message.contains(r#""state":"accepted""#)
        }));

        gateway
            .write(
                r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"chain-stop"}"#,
            )
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted" && message.contains("chain-stop")
            })
            .await;
        gateway.close().await;
        recognizer.finish().await;
        language.finish().await;
        speech.finish().await;
        media.shutdown().await;
    }

    /// A final transcript is still inert under an external gate when every adapter is the
    /// production loopback adapter set; skipping it must not open either remote generation
    /// boundary.
    #[tokio::test]
    async fn online_external_gate_skip_leaves_production_language_and_speech_unstarted() {
        const CREDENTIAL: &str = "CONVERSATION_ONLINE_CHAIN_SKIP_CREDENTIAL";
        std::env::set_var(CREDENTIAL, "fake-not-a-real-credential-0000");
        let recognizer = ScriptedRecognizerServer::two_chunks().await;
        let language = ScriptedHttpProvider::start(language_sse_response("unreachable")).await;
        let speech =
            ScriptedHttpProvider::start(http_response(&pcm16_wav(16_000, 1, 160, 1))).await;
        let recognition_audio = vec![b'o'; 32_000];
        let session_id = SessionId::new(710);
        let mut adapters = loader_built_online_adapters(
            recognizer.endpoint(),
            language.endpoint(),
            speech.endpoint(),
            CREDENTIAL,
        );
        let media = Arc::new(
            MediaServer::bind(adapters.media.take().unwrap())
                .await
                .unwrap(),
        );
        let runtime = conversation_runtime::TextTurnRuntime::new(
            adapters.context,
            Arc::clone(&adapters.language),
        );
        let session = GatewaySession::new(runtime, adapters.status)
            .with_voice(adapters.voice.take().unwrap(), adapters.language)
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"chain-skip-start","response_mode":"external_gate"}"#)
            .await;
        let lease: serde_json::Value = serde_json::from_str(&gateway.read_message().await).unwrap();
        let endpoint: std::net::SocketAddr = lease["media_lease"]["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["media_lease"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(
                session_id,
                1,
                1_000,
                &recognition_audio[..16_000],
            ))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(
                session_id,
                2,
                1_001,
                &recognition_audio[16_000..],
            ))
            .await
            .unwrap();
        recognizer.wait_for_request().await;
        gateway
            .read_until(|message| message.contains(r#""type":"voice_transcript_final""#))
            .await;
        tokio::time::sleep(Duration::from_millis(850)).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"skip_voice_response","request_id":"chain-skip","session_id":"710","segment_id":"1","reason":"meeting policy"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""type":"response_skipped""#))
            .await;
        assert_eq!(recognizer.request_count().await, 2);
        assert_eq!(language.request_count().await, 0);
        assert_eq!(speech.request_count().await, 0);

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"chain-skip-stop"}"#)
            .await;
        gateway
            .read_until(|message| {
                message_type(message) == "command_accepted" && message.contains("chain-skip-stop")
            })
            .await;
        gateway.close().await;
        recognizer.finish().await;
        language.finish().await;
        speech.finish().await;
        media.shutdown().await;
    }

    #[tokio::test]
    async fn online_recognizer_loss_reports_its_stage_without_losing_capture_or_cloud_status() {
        assert_online_recognizer_failure_preserves_recording(false).await;
    }

    #[tokio::test]
    async fn online_recognizer_startup_refusal_preserves_recording_with_zero_audio_upload() {
        assert_online_recognizer_failure_preserves_recording(true).await;
    }

    async fn assert_online_recognizer_failure_preserves_recording(startup_refusal: bool) {
        const CREDENTIAL: &str = "CONVERSATION_ONLINE_LOSS_CREDENTIAL";
        const RECORDING_KEY: &str = "CONVERSATION_ONLINE_LOSS_RECORDING_KEY";
        std::env::set_var(CREDENTIAL, "fake-not-a-real-credential-0000");
        std::env::set_var(RECORDING_KEY, FAKE_RECORDING_KEY);
        let recognizer = if startup_refusal {
            ScriptedRecognizerServer::responding(true, 0).await
        } else {
            ScriptedRecognizerServer::failing().await
        };
        let directory = TempDir::new().unwrap();
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(708);
        let recognition = OpenAiSpeechRecognitionConfig::new(
            recognizer.endpoint(),
            "scripted-asr-model",
            CREDENTIAL,
        )
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let speech = Arc::new(PerRequestSpeech::default());
        let voice = GatewayVoiceAdapters {
            io: GatewayVoiceAdapters::unavailable_online_io(),
            speech: speech.clone(),
            policy: voice_policy(),
            online_recognition: Some(recognition),
        };
        let session = GatewaySession::new(unused_runtime(), online_status_with_voice())
            .with_voice(voice, language.clone())
            .with_recording(RecordingSettings::new(directory.path(), RECORDING_KEY).unwrap())
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;
        gateway.write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"loss-start","response_mode":"external_gate"}"#).await;
        let lease: serde_json::Value = serde_json::from_str(&gateway.read_message().await).unwrap();
        let endpoint: std::net::SocketAddr = lease["media_lease"]["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["media_lease"]["credential"]
            .as_str()
            .unwrap()
            .to_owned();
        gateway
            .read_until(|m| m.contains(r#""type":"voice_session_started""#))
            .await;
        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        bridge
            .write_all(&online_media_capture(session_id, 1, 1_000, b"lost"))
            .await
            .unwrap();
        recognizer.wait_for_request().await;
        let failed = gateway
            .read_until(|m| m.contains(r#""type":"voice_session_failed""#))
            .await;
        assert!(
            failed
                .iter()
                .any(|m| m.contains(r#""stage":"speech_recognizer""#)),
            "{failed:?}"
        );
        assert!(language.requests().is_empty() && speech.requests().is_empty());
        if startup_refusal {
            assert_eq!(recognizer.request_count().await, 0);
        }
        // Confirm actual provider socket closure/reap BEFORE the second media
        // frame. Keeping that socket alive cannot accidentally sustain capture.
        recognizer.finish().await;
        let path = wait_for_recording_bytes(directory.path(), 80).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"loss-status"}"#)
            .await;
        gateway
            .read_until(|m| {
                m.contains(r#""profile":"online""#) && m.contains(r#""privacy_mode":"cloud""#)
            })
            .await;
        bridge
            .write_all(&online_media_capture(session_id, 2, 1_001, b"still!"))
            .await
            .unwrap();
        wait_for_recording_growth(&path).await;
        assert!(language.requests().is_empty() && speech.requests().is_empty());
        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"loss-stop"}"#)
            .await;
        gateway
            .read_until(|m| message_type(m) == "command_accepted" && m.contains("loss-stop"))
            .await;
        let recording = read_session_recording(&path, RECORDING_KEY).unwrap();
        assert!(
            recording.chunks.iter().any(
                |c| matches!(c, RecordedChunk::Frame { sequence: 2, bytes, .. } if bytes == b"still!")
            ),
            "post-failure bridge frame was not recorded: {recording:?}"
        );
        gateway.close().await;
        media.shutdown().await;
    }

    /// Catches a fail-open regression where an explicit online meeting could start a local
    /// voice factory after its selected remote recognizer was absent at startup.
    #[tokio::test]
    async fn online_voice_start_without_recognizer_fails_closed_at_speech_recognizer_without_offline_io(
    ) {
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(709);
        let offline_io = Arc::new(MockVoiceIoFactory::new(Vec::new()));
        let voice = GatewayVoiceAdapters {
            io: offline_io.clone(),
            speech: no_speech_frames(),
            policy: voice_policy(),
            online_recognition: None,
        };
        let session = GatewaySession::new(unused_runtime(), online_status_with_voice())
            .with_voice(voice, test_language())
            .with_media_server(Arc::clone(&media))
            .with_test_session_id(session_id);
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"missing-online-recognizer"}"#)
            .await;
        let rejected = gateway.read_message().await;
        assert!(
            rejected.contains(r#""type":"command_rejected""#),
            "{rejected}"
        );
        assert!(rejected.contains(r#""request_id":"missing-online-recognizer""#));
        assert!(
            rejected.contains(r#""code":"adapter_failure""#),
            "{rejected}"
        );
        assert!(
            rejected.contains(r#""stage":"speech_recognizer""#),
            "{rejected}"
        );
        assert!(!rejected.contains("media_lease"));
        assert_eq!(
            offline_io.start_count(),
            0,
            "online startup used offline I/O"
        );

        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"missing-online-status"}"#)
            .await;
        gateway
            .read_until(|message| {
                message.contains(r#""profile":"online""#)
                    && message.contains(r#""privacy_mode":"cloud""#)
            })
            .await;
        let lease = media.reserve(session_id).await.unwrap();
        media.revoke(lease.session_id()).await;
        gateway.close().await;
        media.shutdown().await;
    }

    /// Catches the production break where a legacy client receives or can mint a
    /// bearer credential it has no protocol field to carry.
    #[tokio::test]
    async fn media_configured_legacy_voice_start_is_request_scoped_and_reserves_nothing() {
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(702);
        let mut gateway = InMemoryGateway::start(media_voice_session(&media, session_id)).await;

        gateway
            .write(r#"{"protocol_version":2,"type":"start_voice_session","request_id":"legacy-media"}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "legacy-media", "invalid_state");
        assert!(!rejection.contains("media_lease"));

        let lease = media.reserve(session_id).await.unwrap();
        media.revoke(lease.session_id()).await;
        gateway.close().await;
        media.shutdown().await;
    }

    /// Catches the production break where a failed reservation produces an
    /// accepted response or replaces the already-authoritative registry entry.
    #[tokio::test]
    async fn media_reservation_failure_rejects_without_disclosing_a_replacement_lease() {
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(703);
        let original = media.reserve(session_id).await.unwrap();
        let mut gateway = InMemoryGateway::start(media_voice_session(&media, session_id)).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"reserve-fail"}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "reserve-fail", "invalid_state");
        assert!(!rejection.contains("media_lease"));

        let mut bridge = TcpStream::connect(original.endpoint()).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, original.credential()))
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        assert!(timeout(Duration::from_millis(100), bridge.read(&mut byte))
            .await
            .is_err());

        media.revoke(session_id).await;
        gateway.close().await;
        media.shutdown().await;
    }

    /// Catches the production break where control-channel EOF reaps the voice
    /// runtime but leaves the bridge credential live in MediaServer.
    #[tokio::test]
    async fn media_v3_client_eof_revokes_the_disclosed_bridge_token() {
        let media = Arc::new(
            MediaServer::bind(MediaServerConfig::loopback())
                .await
                .unwrap(),
        );
        let session_id = SessionId::new(704);
        let mut gateway = InMemoryGateway::start(media_voice_session(&media, session_id)).await;

        gateway
            .write(
                r#"{"protocol_version":3,"type":"start_voice_session","request_id":"eof-media"}"#,
            )
            .await;
        let accepted: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let lease = accepted.get("media_lease").unwrap();
        let endpoint: std::net::SocketAddr = lease["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap()
            .parse()
            .unwrap();
        let credential = lease["credential"].as_str().unwrap().to_owned();

        gateway.close().await;

        let mut bridge = TcpStream::connect(endpoint).await.unwrap();
        bridge
            .write_all(&media_authentication(session_id, &credential))
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(bridge.read(&mut byte).await.unwrap(), 0);
        media.shutdown().await;
    }

    #[tokio::test]
    async fn context_seed_is_atomic_idempotent_recoverable_and_does_not_extract_memory() {
        let (_temporary, store) = initialized_store();
        let extraction = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let context = test_context();
        let runtime = conversation_runtime::TextTurnRuntime::new(
            context.clone(),
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let session = GatewaySession::new(runtime, status()).with_memory_extraction(
            Arc::new(store),
            extraction.clone(),
            Arc::new(FixedClock(timestamp(10_000))),
            crate::MemoryExtractionSettings::new(3, 90),
        );
        let mut gateway = InMemoryGateway::start(session).await;
        let seed = r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-1","operation_id":"continue-1","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#;

        gateway.write(seed).await;
        assert_accepted_message(&gateway.read_message().await, "seed-1");
        assert!(extraction.requests().is_empty());

        let turn = context
            .begin_turn(
                conversation_runtime::ConversationTurnSource::Text,
                "live user",
            )
            .await
            .unwrap();
        assert_eq!(turn.resolved().history_messages()[0].text(), "seed user");
        assert_eq!(
            turn.resolved().history_messages()[1].text(),
            "seed assistant"
        );
        context.discard_turn(turn.identity(), false).await.unwrap();

        gateway.write(seed).await;
        assert_accepted_message(&gateway.read_message().await, "seed-1");

        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-conflict","operation_id":"continue-1","exchanges":[{"user":"different user","assistant":"different assistant"}]}"#,
            )
            .await;
        assert_rejected_message(
            &gateway.read_message().await,
            "seed-conflict",
            "invalid_state",
        );

        gateway
            .write(r#"{"protocol_version":2,"type":"status","request_id":"seed-status"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "seed-status");
        let status = gateway.read_message().await;
        assert!(status.contains(r#""last_context_seed_operation_id":"continue-1""#));
        assert!(extraction.requests().is_empty());

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn context_seed_is_visible_to_the_next_voice_provider_request() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(["voice answer"]));
        let session =
            session_with_interactive_voice(input_receiver, language.clone(), no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-before-voice","operation_id":"continue-before-voice","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "seed-before-voice");

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-after-seed"}"#,
            )
            .await;
        let started = gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;
        assert!(started.iter().any(|message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"voice-after-seed""#)
        }));

        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;
        send_voice_input(
            &input,
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
                RecognitionHypothesis::engine_final(1, "live voice question"),
            )),
        )
        .await;
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 0 }),
        )
        .await;
        tokio::time::advance(Duration::from_millis(900)).await;
        timeout(TEST_TIMEOUT, async {
            while language.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("voice provider request did not start");

        let requests = language.requests();
        let history = requests[0].input().recent_messages();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text(), "seed user");
        assert_eq!(history[1].text(), "seed assistant");

        drop(input);
        let _ = gateway.read_until(is_voice_terminal).await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn context_seed_capability_follows_text_in_the_v2_ready_message() {
        let ready = first_ready_message(GatewaySession::new(unused_runtime(), status())).await;

        assert!(ready.contains(&format!(
            r#""protocol_version":{}"#,
            super::DEFAULT_CLIENT_PROTOCOL_VERSION
        )));
        assert_eq!(
            ready_capabilities(&ready),
            vec!["text".to_owned(), "conversation_context_seed".to_owned()]
        );
    }

    #[tokio::test]
    async fn responses_are_encoded_for_the_protocol_version_the_client_speaks() {
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), status())).await;

        gateway
            .write(r#"{"protocol_version":2,"type":"status","request_id":"status-v2"}"#)
            .await;
        let accepted = frame_value(&gateway.read_message().await);
        assert_eq!(accepted["type"], "command_accepted");
        assert_eq!(accepted["protocol_version"], 2);
        let answered = frame_value(&gateway.read_message().await);
        assert_eq!(answered["type"], "status");
        assert_eq!(answered["protocol_version"], 2);
        let reported = answered["status"].as_object().unwrap();
        assert!(!reported.contains_key("profile"));
        assert!(reported.contains_key("last_context_seed_operation_id"));

        // This session's status names a version 2 capability, so a version 1
        // frame cannot state it: the floor keeps the answer at version 2.
        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-v1"}"#)
            .await;
        assert_eq!(
            frame_value(&gateway.read_message().await)["protocol_version"],
            2
        );
        let floored = frame_value(&gateway.read_message().await);
        assert_eq!(floored["type"], "status");
        assert_eq!(floored["protocol_version"], 2);
        assert!(!floored["status"]
            .as_object()
            .unwrap()
            .contains_key("profile"));

        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"status-v3"}"#)
            .await;
        assert_eq!(
            frame_value(&gateway.read_message().await)["protocol_version"],
            3
        );
        let newest = frame_value(&gateway.read_message().await);
        assert_eq!(newest["type"], "status");
        assert_eq!(newest["protocol_version"], 3);
        assert_eq!(newest["status"]["profile"], "offline");

        gateway.close().await;
    }

    #[tokio::test]
    async fn a_version_one_capable_status_is_answered_at_version_one() {
        let legacy_status = RuntimeStatus {
            capabilities: vec!["text".to_owned()],
            ..status()
        };
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), legacy_status)).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-v1"}"#)
            .await;
        assert_eq!(
            frame_value(&gateway.read_message().await)["protocol_version"],
            1
        );
        let answered = frame_value(&gateway.read_message().await);
        assert_eq!(answered["type"], "status");
        assert_eq!(answered["protocol_version"], 1);
        let reported = answered["status"].as_object().unwrap();
        assert!(!reported.contains_key("profile"));
        assert!(!reported.contains_key("last_context_seed_operation_id"));

        gateway.close().await;
    }

    #[tokio::test]
    async fn context_seed_is_rejected_once_while_a_text_turn_is_active() {
        let language = Arc::new(PendingLanguageModel::default());
        let runtime = conversation_runtime::TextTurnRuntime::new(test_context(), language.clone());
        let mut gateway = InMemoryGateway::start(GatewaySession::new(runtime, status())).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"text-active","transcript":"hold this turn"}"#,
            )
            .await;
        let _ = gateway
            .read_until(|message| message.contains(r#""request_id":"text-active""#))
            .await;
        timeout(TEST_TIMEOUT, language.started.notified())
            .await
            .expect("pending language model did not start");

        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-text-active","operation_id":"continue-text-active","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#,
            )
            .await;
        let messages = gateway
            .read_until(|message| message.contains(r#""request_id":"seed-text-active""#))
            .await;
        let correlated = messages
            .iter()
            .filter(|message| message.contains(r#""request_id":"seed-text-active""#))
            .collect::<Vec<_>>();
        assert_eq!(correlated.len(), 1);
        assert_rejected_message(correlated[0], "seed-text-active", "invalid_state");

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn context_seed_is_rejected_during_voice_start_listening_and_pause() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-seed"}"#,
            )
            .await;
        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-voice-starting","operation_id":"continue-voice-starting","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#,
            )
            .await;
        let starting_messages = gateway
            .read_until(|message| message.contains(r#""request_id":"seed-voice-starting""#))
            .await;
        let starting_response = starting_messages
            .iter()
            .find(|message| message.contains(r#""request_id":"seed-voice-starting""#))
            .unwrap();
        assert_rejected_message(starting_response, "seed-voice-starting", "invalid_state");

        gateway
            .write(
                r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-seed"}"#,
            )
            .await;
        let _ = gateway
            .read_until(|message| message.contains(r#""type":"voice_capture_paused""#))
            .await;
        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-voice-paused","operation_id":"continue-voice-paused","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#,
            )
            .await;
        assert_rejected_message(
            &gateway.read_message().await,
            "seed-voice-paused",
            "invalid_state",
        );

        drop(input);
        let _ = gateway.read_until(is_voice_terminal).await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn context_seed_is_rejected_once_while_voice_startup_is_gated() {
        let (input, input_receiver) = mpsc::channel(8);
        let context = test_context();
        context
            .seed_completed_history(
                "original-startup-history",
                &[conversation_protocol::ConversationContextExchange::new(
                    "original user",
                    "original assistant",
                )
                .unwrap()],
            )
            .await
            .unwrap();
        let runtime = conversation_runtime::TextTurnRuntime::new(
            context.clone(),
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let factory = GatedVoiceIoFactory::new(input_receiver);
        let adapters = GatewayVoiceAdapters {
            io: factory.clone(),
            speech: no_speech_frames(),
            policy: voice_policy(),
            online_recognition: None,
        };
        let session = GatewaySession::new(runtime, status_with_voice()).with_voice(
            adapters,
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-starting-gated"}"#,
            )
            .await;
        timeout(TEST_TIMEOUT, factory.startup_started.wait())
            .await
            .expect("voice I/O startup was never invoked");
        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-during-startup","operation_id":"replacement-during-startup","exchanges":[{"user":"replacement user","assistant":"replacement assistant"}]}"#,
            )
            .await;

        let mut messages = gateway
            .read_until(|message| message.contains(r#""request_id":"seed-during-startup""#))
            .await;
        let correlated = messages
            .iter()
            .filter(|message| message.contains(r#""request_id":"seed-during-startup""#))
            .collect::<Vec<_>>();
        assert_eq!(correlated.len(), 1);
        assert_rejected_message(correlated[0], "seed-during-startup", "invalid_state");
        assert!(!messages
            .iter()
            .any(|message| message.contains(r#""type":"voice_session_started""#)));

        factory.startup_release.set();
        messages.extend(
            gateway
                .read_until(|message| message.contains(r#""type":"voice_session_started""#))
                .await,
        );
        drop(input);
        messages.extend(gateway.read_until(is_voice_terminal).await);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.contains(r#""request_id":"seed-during-startup""#))
                .count(),
            1
        );
        assert_single_history_exchange(&context, "original user", "original assistant").await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn context_seed_is_rejected_once_during_pausing_and_resuming() {
        let (input, input_receiver) = mpsc::channel(8);
        let context = test_context();
        context
            .seed_completed_history(
                "original-control-history",
                &[conversation_protocol::ConversationContextExchange::new(
                    "original user",
                    "original assistant",
                )
                .unwrap()],
            )
            .await
            .unwrap();
        let runtime = conversation_runtime::TextTurnRuntime::new(
            context.clone(),
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let capture = GatedVoiceCaptureControl::new();
        let adapters = GatewayVoiceAdapters {
            io: Arc::new(TestVoiceIoFactory::with_capture(
                input_receiver,
                capture.clone(),
            )),
            speech: no_speech_frames(),
            policy: voice_policy(),
            online_recognition: None,
        };
        let session = GatewaySession::new(runtime, status_with_voice()).with_voice(
            adapters,
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-controls-gated"}"#,
            )
            .await;
        let _ = gateway
            .read_until(|message| message.contains(r#""type":"voice_session_started""#))
            .await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-gated"}"#,
            )
            .await;
        timeout(TEST_TIMEOUT, capture.pause_started.wait())
            .await
            .expect("pause transition never started");
        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-during-pausing","operation_id":"replacement-during-pausing","exchanges":[{"user":"pausing replacement","assistant":"pausing replacement answer"}]}"#,
            )
            .await;
        let pausing = gateway
            .read_until(|message| message.contains(r#""request_id":"seed-during-pausing""#))
            .await;
        let pausing_correlated = pausing
            .iter()
            .filter(|message| message.contains(r#""request_id":"seed-during-pausing""#))
            .collect::<Vec<_>>();
        assert_eq!(pausing_correlated.len(), 1);
        assert_rejected_message(
            pausing_correlated[0],
            "seed-during-pausing",
            "invalid_state",
        );

        capture.pause_release.set();
        let _ = gateway
            .read_until(|message| message.contains(r#""type":"voice_capture_paused""#))
            .await;
        gateway
            .write(
                r#"{"protocol_version":1,"type":"resume_voice_capture","request_id":"resume-gated"}"#,
            )
            .await;
        timeout(TEST_TIMEOUT, capture.resume_started.wait())
            .await
            .expect("resume transition never started");
        gateway
            .write(
                r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-during-resuming","operation_id":"replacement-during-resuming","exchanges":[{"user":"resuming replacement","assistant":"resuming replacement answer"}]}"#,
            )
            .await;
        let resuming = gateway
            .read_until(|message| message.contains(r#""request_id":"seed-during-resuming""#))
            .await;
        let resuming_correlated = resuming
            .iter()
            .filter(|message| message.contains(r#""request_id":"seed-during-resuming""#))
            .collect::<Vec<_>>();
        assert_eq!(resuming_correlated.len(), 1);
        assert_rejected_message(
            resuming_correlated[0],
            "seed-during-resuming",
            "invalid_state",
        );

        capture.resume_release.set();
        let _ = gateway
            .read_until(|message| message.contains(r#""type":"voice_capture_resumed""#))
            .await;
        drop(input);
        let _ = gateway.read_until(is_voice_terminal).await;
        assert_single_history_exchange(&context, "original user", "original assistant").await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn context_seed_queued_during_voice_stop_rejects_while_terminal_is_pending() {
        let (input, input_receiver) = mpsc::channel(8);
        let context = test_context();
        context
            .seed_completed_history(
                "original-stop-history",
                &[conversation_protocol::ConversationContextExchange::new(
                    "original user",
                    "original assistant",
                )
                .unwrap()],
            )
            .await
            .unwrap();
        let runtime = conversation_runtime::TextTurnRuntime::new(
            context.clone(),
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        );
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let adapters = GatewayVoiceAdapters {
            io: Arc::new(TestVoiceIoFactory::with_capture(
                input_receiver,
                Arc::new(MockVoiceCaptureControl::new()),
            )),
            speech: no_speech_frames(),
            policy: voice_policy(),
            online_recognition: None,
        };
        let session =
            GatewaySession::new(runtime, status_with_voice()).with_voice(adapters, language);
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
            r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"seed-stopping","operation_id":"continue-stopping","exchanges":[{"user":"seed user","assistant":"seed assistant"}]}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&normal_monitor) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("seed queued during stop did not receive a response");

        drop(urgent_monitor);
        drop(normal_monitor);
        drop(event_monitor);
        writer_state.release();
        drop(input);
        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not close")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let correlated = messages
            .iter()
            .filter(|message| message.contains(r#""request_id":"seed-stopping""#))
            .collect::<Vec<_>>();
        assert_eq!(correlated.len(), 1);
        assert_rejected_message(correlated[0], "seed-stopping", "invalid_state");
        let terminal = message_index(&messages, is_voice_terminal);
        let stop_accepted = message_index(&messages, |message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"stop-1""#)
        });
        let seed_rejected = message_index(&messages, |message| {
            message.contains(r#""request_id":"seed-stopping""#)
        });
        assert!(terminal < stop_accepted);
        assert!(stop_accepted < seed_rejected);
        assert_single_history_exchange(&context, "original user", "original assistant").await;
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

    // Pipelined control requests while stdout is stalled must wait for the writer, never end
    // the gateway session (and with it an active recording) on lane backpressure.
    #[tokio::test]
    async fn pipelined_status_requests_during_a_stdout_stall_wait_instead_of_ending_the_session() {
        let session = GatewaySession::new(unused_runtime(), status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = BlockingWriter::new(0);
        let session_task = tokio::spawn(session.run(reader, writer));
        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer never stalled");
        for id in ["s1", "s2", "s3", "s4", "s5"] {
            write_command(
                &mut input,
                &format!(r#"{{"protocol_version":3,"type":"status","request_id":"{id}"}}"#),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !session_task.is_finished(),
            "control-reply backpressure ended the gateway session"
        );
        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"s5","status""#),
        )
        .await
        .expect("every pipelined status reply is written once stdout resumes");
        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not end at input EOF")
            .unwrap()
            .unwrap();
        let messages = decode_frames(&writer_state.bytes());
        for id in ["s1", "s2", "s3", "s4", "s5"] {
            let request = format!(r#""request_id":"{id}""#);
            let accepted = message_index(&messages, |message| {
                message_type(message) == "command_accepted" && message.contains(&request)
            });
            let answered = message_index(&messages, |message| {
                message_type(message) == "status" && message.contains(&request)
            });
            assert!(accepted < answered);
        }
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
    async fn normal_control_saturation_waits_for_output_and_keeps_the_session() {
        let language = HoldOpenLanguageServer::start().await;
        let runtime = runtime_for(language.endpoint());
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

        // Saturated control replies wait for stdout instead of failing the session.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !session_task.is_finished(),
            "normal control saturation ended the gateway session"
        );
        assert!(!language.connection_reaped.is_set());
        assert!(writer_state.blocked.is_set());

        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"status-saturation-7","status""#),
        )
        .await
        .expect("saturated status replies were not written after stdout resumed");
        drop(input);
        let result = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("saturated gateway session did not finish")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("input EOF did not reap the active generation");
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
                media_lease: None,
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
                        media_lease: None,
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
            Arc::new(std::sync::atomic::AtomicU64::new(CLIENT_PROTOCOL_VERSION)),
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
        let voice_language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let text_language = Arc::new(PendingLanguageModel::default());
        let text_runtime =
            conversation_runtime::TextTurnRuntime::new(test_context(), text_language.clone());
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
        timeout(TEST_TIMEOUT, text_language.started.notified())
            .await
            .expect("text turn did not remain active for the resume rejection");
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
        text_language.release.notify_one();
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

    /// Under a stalled stdout with queued control replies, stop and finalize are still read
    /// and processed within a bound, and input EOF still ends the session.
    #[tokio::test]
    async fn stop_finalize_and_eof_are_processed_within_a_bound_while_replies_are_stalled() {
        let (_input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let (session, cancelled) = session_with_interactive_voice_and_cancellation(
            input_receiver,
            language,
            no_speech_frames(),
        );
        let (mut client_input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
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
        for id in ["s1", "s2", "s3"] {
            write_command(
                &mut client_input,
                &format!(r#"{{"protocol_version":1,"type":"status","request_id":"{id}"}}"#),
            )
            .await;
        }
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, cancelled.wait())
            .await
            .expect("stop was not processed while replies were stalled");
        // Commands are handled in input order, so the session ending at the EOF that
        // follows proves finalize was read and handled too.
        write_command(
            &mut client_input,
            r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
        )
        .await;
        drop(client_input);
        let _ = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("input EOF was not observed while replies were stalled");
        assert!(!writer_state.released.load(Ordering::SeqCst));
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

    #[test]
    fn a_voice_reply_pairs_with_the_utterance_from_its_own_turn() {
        let mut exchange = VoiceExchange::default();

        exchange.recognized(TurnId::new(1), "i ship rust");

        assert_eq!(
            exchange.completed(TurnId::new(1)),
            Some("i ship rust".to_owned())
        );
        assert_eq!(exchange.completed(TurnId::new(1)), None);
    }

    #[test]
    fn a_voice_reply_from_another_turn_leaves_the_utterance_for_its_own_turn() {
        let mut exchange = VoiceExchange::default();

        // The user spoke again (turn 2) before turn 1's reply finished.
        exchange.recognized(TurnId::new(2), "and i review on fridays");

        assert_eq!(exchange.completed(TurnId::new(1)), None);
        assert_eq!(
            exchange.completed(TurnId::new(2)),
            Some("and i review on fridays".to_owned()),
            "the mismatched reply discarded an utterance that was still waiting for its own turn"
        );
    }

    #[test]
    fn a_voice_reply_without_a_recognized_utterance_pairs_nothing() {
        assert_eq!(VoiceExchange::default().completed(TurnId::new(1)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_voice_exchange_reports_extracted_memories() {
        let (_temporary, store) = initialized_store();
        let (input, input_receiver) = mpsc::channel(8);
        let turn_language = Arc::new(MockGenerationLanguageModel::new(["i see"]));
        let extraction = MockGenerationLanguageModel::new([
            r#"[{"kind":"semantic","content":"the user ships rust","explicit":true,"confidence":800}]"#,
        ]);
        let adapters = GatewayVoiceAdapters {
            io: Arc::new(TestVoiceIoFactory::with_capture(
                input_receiver,
                Arc::new(MockVoiceCaptureControl::new()),
            )),
            speech: Arc::new(PerRequestSpeech::default()),
            policy: voice_policy(),
            online_recognition: None,
        };
        let session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(adapters, turn_language)
            .with_memory_extraction(
                Arc::new(store.clone()),
                Arc::new(extraction),
                Arc::new(FixedClock(timestamp(10_000))),
                crate::MemoryExtractionSettings::new(3, 90),
            );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-extract"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-extract");

        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;
        send_voice_input(
            &input,
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
                RecognitionHypothesis::engine_final(1, "i ship rust"),
            )),
        )
        .await;
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 0 }),
        )
        .await;
        tokio::time::advance(Duration::from_millis(900)).await;
        // Real time from here: the extraction's own store write runs on the blocking
        // pool, and a paused clock would otherwise auto-advance the reader's deadline
        // while the runtime waits on it.
        tokio::time::resume();

        let messages = gateway
            .read_until(|message| message_type(message) == "memory_extracted")
            .await;

        assert!(messages
            .iter()
            .any(|message| message.contains(r#""type":"voice_transcript_final""#)));
        let extracted = messages.last().unwrap();
        assert!(extracted.contains(r#""created":1"#), "{extracted}");
        assert!(extracted.contains(r#""activated":1"#), "{extracted}");
        let records = store.list(timestamp(10_000)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content(), "the user ships rust");

        drop(input);
        gateway.read_until(is_voice_terminal).await;
        gateway.close().await;
    }

    /// Answers every synthesis request with one silent frame carrying that request's own
    /// identities, so a voice turn reaches its completed terminal (and therefore emits
    /// `TextCompleted`) instead of failing in synthesis the way `no_speech_frames` does.
    #[derive(Default)]
    struct PerRequestSpeech {
        requests: StdMutex<Vec<StreamingSpeechRequest>>,
    }

    impl PerRequestSpeech {
        fn requests(&self) -> Vec<StreamingSpeechRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl StreamingSpeechSynthesizer for PerRequestSpeech {
        fn stream(
            &self,
            request: StreamingSpeechRequest,
            _cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
            self.requests.lock().unwrap().push(request.clone());
            let frame = AudioFrame::new(
                request.turn_id(),
                request.generation_id(),
                request.utterance_id(),
                0,
                PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                vec![0; 960],
            )
            .unwrap();
            let (sender, receiver) = mpsc::channel(1);
            tokio::spawn(async move {
                let _ = sender.send(Ok(frame)).await;
            });
            receiver
        }
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

    #[derive(Default)]
    struct PendingLanguageModel {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl GenerationLanguageModel for PendingLanguageModel {
        fn stream(
            &self,
            request: GenerationLanguageRequest,
            cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
            let (sender, receiver) = mpsc::channel(1);
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            tokio::spawn(async move {
                started.notify_one();
                tokio::select! {
                    _ = cancellation.cancelled() => {}
                    _ = release.notified() => {
                        let _ = sender.send(Ok(GenerationTextDelta::new(
                            request.turn_id(),
                            request.generation_id(),
                            "completed response",
                        ))).await;
                    }
                }
                drop(sender);
            });
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
        assert!(
            message.contains(r#""type":"command_rejected""#),
            "expected command rejection, received {message}"
        );
        assert!(
            message.contains(&format!(r#""request_id":"{request_id}""#)),
            "expected request {request_id}, received {message}"
        );
        assert!(
            message.contains(&format!(r#""code":"{code}""#)),
            "expected error code {code}, received {message}"
        );
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
            profile: "offline".to_owned(),
            transport: "stdio".to_owned(),
            privacy_mode: "local_only".to_owned(),
            language_location: "local".to_owned(),
            model_id: "test-model".to_owned(),
            memory_enabled: false,
            memory_location: None,
            telemetry_enabled: false,
            capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
            components: vec![conversation_protocol::ClientComponentDescriptor {
                kind: "language_model".to_owned(),
                execution_location: "local".to_owned(),
                provider_label: "test-language".to_owned(),
            }],
            last_context_seed_operation_id: None,
        }
    }

    fn memory_status() -> RuntimeStatus {
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("local".to_owned()),
            capabilities: vec![
                "text".to_owned(),
                "conversation_context_seed".to_owned(),
                "memory_inspection".to_owned(),
            ],
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
            capabilities: vec![
                "text".to_owned(),
                "conversation_context_seed".to_owned(),
                "voice_session".to_owned(),
            ],
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

    fn online_status_with_voice() -> RuntimeStatus {
        let mut status = status_with_voice();
        status.profile = "online".to_owned();
        status.privacy_mode = "cloud".to_owned();
        status.language_location = "remote".to_owned();
        for component in &mut status.components {
            if component.kind != "audio_io" {
                component.execution_location = "remote".to_owned();
            }
        }
        status
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
            online_recognition: None,
        };
        (adapters, VoiceTestGuards { io_factory, speech })
    }

    fn session_with_voice() -> (GatewaySession, VoiceTestGuards) {
        let (voice_adapters, guards) = test_voice_adapters();
        let session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(voice_adapters, test_language());
        (session, guards)
    }

    fn media_voice_session(media: &Arc<MediaServer>, session_id: SessionId) -> GatewaySession {
        let (voice, _guards) = test_voice_adapters();
        GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(voice, test_language())
            .with_media_server(Arc::clone(media))
            .with_test_session_id(session_id)
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
                self.receiver
                    .lock()
                    .expect("test voice input receiver lock poisoned")
                    .take()
                    .ok_or_else(|| AdapterError::new("test voice input started more than once"))
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

    struct GatedVoiceIoFactory {
        inner: TestVoiceIoFactory,
        startup_started: Condition,
        startup_release: Condition,
    }

    impl GatedVoiceIoFactory {
        fn new(input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>) -> Arc<Self> {
            Arc::new(Self {
                inner: TestVoiceIoFactory::with_capture(
                    input_receiver,
                    Arc::new(MockVoiceCaptureControl::new()),
                ),
                startup_started: Condition::default(),
                startup_release: Condition::default(),
            })
        }
    }

    impl VoiceIoFactory for GatedVoiceIoFactory {
        fn start<'a>(
            &'a self,
            session_id: SessionId,
            cancellation: CancellationToken,
        ) -> AdapterFuture<'a, VoiceIoSession> {
            Box::pin(async move {
                self.startup_started.set();
                self.startup_release.wait().await;
                self.inner.start(session_id, cancellation).await
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
            online_recognition: None,
        };
        GatewaySession::new(unused_runtime(), status_with_voice()).with_voice(adapters, language)
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
            online_recognition: None,
        };
        let session = GatewaySession::new(unused_runtime(), status_with_voice())
            .with_voice(adapters, language);
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
            online_recognition: None,
        };
        GatewaySession::new(runtime, status_with_voice()).with_voice(adapters, language)
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

    async fn assert_single_history_exchange(
        context: &conversation_runtime::ConversationContext,
        user: &str,
        assistant: &str,
    ) {
        let turn = context
            .begin_turn(
                conversation_runtime::ConversationTurnSource::Text,
                "history inspection probe",
            )
            .await
            .unwrap();
        let history = turn.resolved().history_messages();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text(), user);
        assert_eq!(history[1].text(), assistant);
        context.discard_turn(turn.identity(), false).await.unwrap();
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

    fn media_authentication(session_id: SessionId, credential: &str) -> Vec<u8> {
        let mut bytes = vec![b'M', b'E', b'D', b'1', 1, 1];
        bytes.extend_from_slice(&session_id.get().to_be_bytes());
        bytes.push(credential.len().try_into().unwrap());
        bytes.extend_from_slice(credential.as_bytes());
        bytes
    }

    fn media_capture(
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        media_capture_at_rate(session_id, sequence, captured_at_ms, payload, 16_000)
    }

    fn online_media_capture(
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        media_capture_at_rate(session_id, sequence, captured_at_ms, payload, 24_000)
    }

    fn media_capture_at_rate(
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
        payload: &[u8],
        sample_rate: u32,
    ) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(42 + payload.len());
        bytes.extend_from_slice(b"MED1\x01\x02\x01");
        bytes.extend_from_slice(&session_id.get().to_be_bytes());
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&captured_at_ms.to_be_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&sample_rate.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
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

    struct ProviderRequest {
        target: String,
        payload: serde_json::Value,
        authorization: Option<String>,
    }

    /// A single-process HTTP boundary for the production online adapters. It deliberately
    /// records complete JSON requests while the gateway keeps the adapters themselves real.
    struct ScriptedHttpProvider {
        endpoint: String,
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ScriptedHttpProvider {
        async fn start(response: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let worker_requests = Arc::clone(&requests);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let Ok(request) = read_provider_request(&mut stream).await else {
                        continue;
                    };
                    worker_requests.lock().await.push(request);
                    if stream.write_all(&response).await.is_err() {
                        continue;
                    }
                    let _ = stream.flush().await;
                }
            });
            Self {
                endpoint,
                requests,
                task,
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        async fn request_count(&self) -> usize {
            self.requests.lock().await.len()
        }

        async fn request(&self, index: usize) -> ProviderRequest {
            let requests = self.requests.lock().await;
            ProviderRequest {
                target: requests[index].target.clone(),
                payload: requests[index].payload.clone(),
                authorization: requests[index].authorization.clone(),
            }
        }

        async fn wait_for_request(&self) {
            timeout(TEST_TIMEOUT, async {
                loop {
                    if self.request_count().await > 0 {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("scripted online provider did not receive a request");
        }

        async fn finish(mut self) {
            self.task.abort();
            assert!((&mut self.task)
                .await
                .as_mut()
                .is_err_and(|error| error.is_cancelled()));
        }
    }

    impl Drop for ScriptedHttpProvider {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_provider_request(stream: &mut TcpStream) -> Result<ProviderRequest, ()> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.map_err(|_| ())?;
            if read == 0 {
                return Err(());
            }
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .ok_or(())?;
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.map_err(|_| ())?;
            if read == 0 {
                return Err(());
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let target = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or(())?
            .to_owned();
        let authorization = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_owned())
        });
        let payload = serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .map_err(|_| ())?;
        Ok(ProviderRequest {
            target,
            payload,
            authorization,
        })
    }

    /// Builds the exact online adapter set from the gateway configuration path. The TOML keeps
    /// its deployable HTTPS endpoints; only the test-only builder seam replaces the already
    /// validated adapter transport targets with isolated loopback providers.
    fn loader_built_online_adapters(
        recognition_endpoint: &str,
        language_endpoint: &str,
        speech_endpoint: &str,
        credential_env: &str,
    ) -> crate::GatewayAdapters {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("online-loader-chain.toml");
        std::fs::write(
            &path,
            format!(
                r#"schema_version = 3
profile = "online"
privacy_mode = "cloud"

[language]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-language"
endpoint = "https://provider.example.invalid/v1"
model = "scripted-language-model"
credential_env = "{credential_env}"
thinking = false
temperature = 0.7
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[persona]
mode = "direct-answer"
warmth = 80
humor = 60
teasing = 40
initiative = 35
directness = 80
intimacy = 30
verbosity = 20
follow_up_frequency = 25

[voice]
response_mode = "external_gate"

[media]
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "openai-streaming"
execution = "remote"
provider = "scripted-online-asr"
endpoint = "wss://provider.example.invalid/v1/realtime?intent=transcription"
model = "scripted-asr-model"
credential_env = "{credential_env}"

[voice.asr.server_vad]
threshold = 0.5
prefix_padding_ms = 300
silence_duration_ms = 500

[voice.speech]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-speech"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://provider.example.invalid/v1"
model = "scripted-speech-model"
voice = "scripted-voice"
credential_env = "{credential_env}"
speed = 1.0
language = "auto"
instructions = "Speak clearly."
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "scripted-local-audio"
sidecar_executable = "/absolute/path/to/unused-sidecar"
max_error_bytes = 65536
"#,
            ),
        )
        .unwrap();
        let normal = crate::GatewayConfig::load(&path).unwrap();
        assert_eq!(normal.status.profile, "online");
        assert_eq!(normal.status.privacy_mode, "cloud");
        assert!(normal.voice.is_some());
        crate::GatewayConfig::load_with_test_provider_endpoints(
            &path,
            crate::config::GatewayTestProviderEndpoints::new(
                format!("{language_endpoint}/v1"),
                recognition_endpoint.to_owned(),
                format!("{speech_endpoint}/v1"),
            ),
        )
        .unwrap()
    }

    fn language_sse_response(text: &str) -> Vec<u8> {
        let first = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})
        );
        let done = "data: [DONE]\n\n";
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for chunk in [first.as_bytes(), done.as_bytes()] {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    }

    fn http_response(body: &[u8]) -> Vec<u8> {
        http_response_with_content_type(body, "application/octet-stream")
    }

    fn http_response_with_content_type(body: &[u8], content_type: &str) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn pcm16_wav(sample_rate_hz: u32, channels: u16, sample_count: usize, sample: i16) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(sample_count * usize::from(channels) * 2);
        for _ in 0..sample_count * usize::from(channels) {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let data_bytes = u32::try_from(pcm.len()).unwrap();
        let block_align = channels * 2;
        let byte_rate = sample_rate_hz * u32::from(block_align);
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        wav.extend_from_slice(&pcm);
        wav
    }

    async fn read_loopback_playback(
        stream: &mut TcpStream,
        session_id: SessionId,
        expected_pcm: &[u8],
    ) -> u64 {
        let mut header = [0_u8; 66];
        timeout(TEST_TIMEOUT, stream.read_exact(&mut header))
            .await
            .expect("the production streaming speech never reached the media bridge")
            .unwrap();
        assert_eq!(&header[..7], b"MED1\x01\x02\x02");
        assert_eq!(
            u64::from_be_bytes(header[7..15].try_into().unwrap()),
            session_id.get()
        );
        assert_ne!(u64::from_be_bytes(header[23..31].try_into().unwrap()), 0);
        assert_ne!(u64::from_be_bytes(header[31..39].try_into().unwrap()), 0);
        assert_ne!(u64::from_be_bytes(header[39..47].try_into().unwrap()), 0);
        assert_eq!(u64::from_be_bytes(header[47..55].try_into().unwrap()), 0);
        assert_eq!(header[55], 1);
        assert_eq!(
            u32::from_be_bytes(header[56..60].try_into().unwrap()),
            16_000
        );
        assert_eq!(u16::from_be_bytes(header[60..62].try_into().unwrap()), 1);
        let byte_length = u32::from_be_bytes(header[62..66].try_into().unwrap()) as usize;
        assert_eq!(byte_length, expected_pcm.len() - 44);
        let mut actual_pcm = vec![0_u8; byte_length];
        stream.read_exact(&mut actual_pcm).await.unwrap();
        assert_eq!(actual_pcm, expected_pcm[44..]);
        u64::from_be_bytes(header[15..23].try_into().unwrap())
    }

    async fn send_loopback_playback_ack(
        stream: &mut TcpStream,
        session_id: SessionId,
        playback_sequence: u64,
    ) {
        send_loopback_playback_state(stream, session_id, playback_sequence, 1).await;
    }

    async fn send_loopback_playback_state(
        stream: &mut TcpStream,
        session_id: SessionId,
        playback_sequence: u64,
        state: u8,
    ) {
        stream.write_all(b"MED1\x01\x03").await.unwrap();
        stream
            .write_all(&session_id.get().to_be_bytes())
            .await
            .unwrap();
        stream
            .write_all(&playback_sequence.to_be_bytes())
            .await
            .unwrap();
        stream.write_u8(state).await.unwrap();
        stream.flush().await.unwrap();
    }

    struct ScriptedRecognizerServer {
        endpoint: String,
        request_seen: Arc<Condition>,
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        update: Arc<Mutex<Option<serde_json::Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ScriptedRecognizerServer {
        async fn start() -> Self {
            Self::responding(false, 1).await
        }
        async fn failing() -> Self {
            Self::responding(true, 1).await
        }
        async fn two_chunks() -> Self {
            Self::responding(false, 2).await
        }

        async fn responding(fail: bool, chunks: usize) -> Self {
            use futures_util::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::{
                handshake::server::{Callback, ErrorResponse, Request, Response},
                Message,
            };
            struct CaptureHandshake<'a>(&'a mut Option<(String, Option<String>)>);
            impl Callback for CaptureHandshake<'_> {
                fn on_request(
                    self,
                    request: &Request,
                    response: Response,
                ) -> Result<Response, ErrorResponse> {
                    *self.0 = Some((
                        request.uri().path_and_query().unwrap().as_str().to_owned(),
                        request
                            .headers()
                            .get("authorization")
                            .map(|value| value.to_str().unwrap().to_owned()),
                    ));
                    Ok(response)
                }
            }
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "ws://{}/v1/realtime?intent=transcription",
                listener.local_addr().unwrap()
            );
            let request_seen = Arc::new(Condition::default());
            let observed = request_seen.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let received = requests.clone();
            let update = Arc::new(Mutex::new(None));
            let received_update = update.clone();
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut handshake = None;
                let mut socket =
                    tokio_tungstenite::accept_hdr_async(stream, CaptureHandshake(&mut handshake))
                        .await
                        .unwrap();
                let (target, authorization) = handshake.unwrap();
                socket.send(Message::Text(serde_json::json!({"type":"session.created","session":{"type":"transcription"}}).to_string().into())).await.unwrap();
                let message = socket.next().await.unwrap().unwrap();
                let update: serde_json::Value =
                    serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(update["type"], "session.update");
                assert_eq!(
                    update["session"]["audio"]["input"]["format"],
                    serde_json::json!({"type":"audio/pcm","rate":24000})
                );
                *received_update.lock().await = Some(update.clone());
                let mut confirmed = update["session"].clone();
                if chunks == 0 {
                    confirmed["audio"]["input"]["transcription"]["model"] =
                        serde_json::json!("wrong-model");
                }
                socket
                    .send(Message::Text(
                        serde_json::json!({"type":"session.updated","session":confirmed})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                for _ in 0..chunks {
                    let message = socket.next().await.unwrap().unwrap();
                    let payload: serde_json::Value =
                        serde_json::from_str(message.to_text().unwrap()).unwrap();
                    assert_eq!(payload["type"], "input_audio_buffer.append");
                    received.lock().await.push(ProviderRequest {
                        target: target.clone(),
                        authorization: authorization.clone(),
                        payload,
                    });
                }
                observed.set();
                let events = if chunks == 0 {
                    vec![]
                } else if fail {
                    vec![
                        serde_json::json!({"type":"error","error":{"message":"scripted unavailable"}}),
                    ]
                } else {
                    vec![
                        serde_json::json!({"type":"input_audio_buffer.speech_started","item_id":"provider-A","audio_start_ms":0}),
                        serde_json::json!({"type":"input_audio_buffer.speech_stopped","item_id":"provider-A","audio_end_ms":1}),
                        serde_json::json!({"type":"input_audio_buffer.committed","item_id":"provider-A","previous_item_id":null}),
                        serde_json::json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"provider-A","content_index":0,"transcript":"agenda item"}),
                    ]
                };
                for event in events {
                    socket
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                while let Some(Ok(message)) = socket.next().await {
                    assert!(
                        !(chunks == 0 && matches!(message, Message::Text(_))),
                        "startup refusal uploaded audio"
                    );
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
            });
            Self {
                endpoint,
                request_seen,
                requests,
                update,
                task,
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }
        async fn request_count(&self) -> usize {
            self.requests.lock().await.len()
        }
        async fn request(&self, index: usize) -> ProviderRequest {
            let requests = self.requests.lock().await;
            let request = &requests[index];
            ProviderRequest {
                target: request.target.clone(),
                payload: request.payload.clone(),
                authorization: request.authorization.clone(),
            }
        }
        async fn update(&self) -> serde_json::Value {
            self.update.lock().await.clone().unwrap()
        }
        async fn wait_for_request(&self) {
            timeout(TEST_TIMEOUT, self.request_seen.wait())
                .await
                .expect("online recognizer never received the bridge capture");
        }
        async fn finish(mut self) {
            match timeout(TEST_TIMEOUT, &mut self.task).await {
                Ok(result) => result.unwrap(),
                Err(_) => {
                    self.task.abort();
                    let _ = self.task.await;
                    panic!("scripted recognition socket did not reap");
                }
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

    fn frame_value(message: &str) -> serde_json::Value {
        serde_json::from_str(message).expect("gateway frames are JSON objects")
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

    // --- session recording over the control channel ---------------------------------------

    /// A fixed fake 256-bit key on a variable this test process owns, hexadecimal as the sink
    /// requires. No real key exists anywhere here.
    const FAKE_RECORDING_KEY: &str =
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn recording_frame(sequence: u64) -> VoiceInputEvent {
        VoiceInputEvent::Capture(CaptureEvent::Frame(
            AudioFrame::new(
                TurnId::new(1),
                GenerationId::new(1),
                UtteranceId::new(1),
                sequence,
                PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                vec![0x21, 0x43, 0x65, 0x87],
            )
            .unwrap(),
        ))
    }

    /// Waits until the recording on disk has grown past `minimum` bytes, so a test never
    /// stops a session before the media path has written what it was given.
    async fn wait_for_recording_bytes(
        directory: &std::path::Path,
        minimum: u64,
    ) -> std::path::PathBuf {
        timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(entry) = std::fs::read_dir(directory)
                    .ok()
                    .and_then(|mut entries| entries.next())
                    .and_then(Result::ok)
                {
                    if entry.metadata().map(|metadata| metadata.len()).unwrap_or(0) >= minimum {
                        return entry.path();
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the recording never reached the expected size")
    }

    async fn wait_for_recording_growth(path: &std::path::Path) {
        let initial = std::fs::metadata(path).unwrap().len();
        timeout(TEST_TIMEOUT, async {
            loop {
                if std::fs::metadata(path)
                    .map(|metadata| metadata.len() > initial)
                    .unwrap_or(false)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recording did not receive the post-failure media frame before timeout");
    }

    async fn recorded_gateway(
        directory: &TempDir,
        key_env: &str,
        input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    ) -> InMemoryGateway {
        let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
        let session = session_with_interactive_voice(
            input_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        )
        .with_recording(settings);
        InMemoryGateway::start(session).await
    }

    #[tokio::test]
    async fn finalize_recording_returns_the_handle_over_the_control_channel() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_HANDLE";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let (input, input_receiver) = mpsc::channel(8);
        let mut gateway = recorded_gateway(&directory, key_env, input_receiver).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        send_voice_input(&input, recording_frame(1)).await;
        send_voice_input(&input, recording_frame(2)).await;
        let path = wait_for_recording_bytes(directory.path(), 128).await;

        // Finalizing under the running media path would close the sink and strand the rest of
        // the meeting, so it is refused while the session is live.
        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"early","session_id":"1"}"#,
            )
            .await;
        let early = gateway
            .read_until(|message| message.contains(r#""request_id":"early""#))
            .await;
        let rejection = early
            .last()
            .expect("read_until returns the frame it matched");
        assert_rejected_message(rejection, "early", "invalid_state");
        assert!(
            rejection.contains("the voice session is still recording"),
            "the rejection names its own cause: {rejection}"
        );

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        let messages = gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;
        assert!(messages.iter().any(|message| is_voice_terminal(message)));

        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "fin-1");
        let finalized = gateway.read_message().await;
        assert!(finalized.contains(r#""type":"recording_finalized""#));
        assert!(finalized.contains(r#""request_id":"fin-1""#));
        assert!(finalized.contains(r#""key_id":"#));
        assert!(
            !finalized.contains(FAKE_RECORDING_KEY),
            "the control channel must never carry the key: {finalized}"
        );
        assert!(
            !finalized.contains(key_env),
            "the control channel must not even name the key variable: {finalized}"
        );

        let recording: serde_json::Value = serde_json::from_str(&finalized).unwrap();
        let handle = recording["recording"]["recording_handle"].as_str().unwrap();
        assert_eq!(handle.len(), 32);
        assert_eq!(
            recording["recording"]["encrypted_path"].as_str().unwrap(),
            path.to_str().unwrap()
        );
        assert_eq!(
            resolve_recording_handle(directory.path(), handle).unwrap(),
            path
        );
        // The Meeting Agent copies by handle; the key stays on the runtime host.
        let decrypted = read_session_recording(&path, key_env).unwrap();
        assert_eq!(decrypted.session_id, SessionId::new(1));

        gateway.close().await;
    }

    #[tokio::test]
    async fn a_start_is_refused_until_the_previous_recording_is_finalized() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_UNFINALIZED_START";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let (input, input_receiver) = mpsc::channel(8);
        let mut gateway = recorded_gateway(&directory, key_env, input_receiver).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        send_voice_input(&input, recording_frame(1)).await;
        wait_for_recording_bytes(directory.path(), 64).await;
        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;

        // Starting again would replace the only reference to the first recording's handle.
        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-2"}"#)
            .await;
        let refused = gateway
            .read_until(|message| message.contains(r#""request_id":"voice-2""#))
            .await;
        let refused = refused
            .last()
            .expect("read_until returns the frame it matched");
        assert_rejected_message(refused, "voice-2", "invalid_state");
        assert!(
            refused.contains("the previous recording has not been finalized"),
            "{refused}"
        );

        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "fin-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"recording_finalized""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn a_refused_v4_finalize_still_closes_an_incomplete_recording_so_voice_can_restart() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_INCOMPLETE_V4";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let settings = RecordingSettings::new(directory.path(), key_env)
            .unwrap()
            .with_test_sink_fault(conversation_runtime::RecordingSinkFault::Write);
        let (input, input_receiver) = mpsc::channel(8);
        let session = session_with_interactive_voice(
            input_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        )
        .with_recording(settings)
        .with_v4_control();
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":4,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"voice-1""#))
            .await;
        // The first recording write fails: the recording is incomplete from here on.
        send_voice_input(&input, recording_frame(1)).await;
        gateway
            .read_until(|message| message.contains(r#""type":"voice_session_failed""#))
            .await;
        gateway
            .write(r#"{"protocol_version":4,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;

        // An incomplete recording cannot authorize recovery, so the v4 finalize is refused...
        gateway
            .write(
                r#"{"protocol_version":4,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
            )
            .await;
        let refused = gateway
            .read_until(|message| message.contains(r#""request_id":"fin-1""#))
            .await;
        assert!(
            refused
                .last()
                .is_some_and(|message| message.contains("command_rejected")),
            "{refused:?}"
        );
        // ...but the recording is closed, so the session is not locked out of voice for good.
        gateway
            .write(r#"{"protocol_version":4,"type":"start_voice_session","request_id":"voice-2"}"#)
            .await;
        let started = gateway
            .read_until(|message| message.contains(r#""request_id":"voice-2""#))
            .await;
        let started = started.last().unwrap();
        assert!(
            started.contains("command_accepted"),
            "a new voice session starts after the refused finalize: {started}"
        );
        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn gateway_exit_finalizes_a_recording_the_client_never_finalized() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_EXIT_FINALIZE";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let (input, input_receiver) = mpsc::channel(8);
        let mut gateway = recorded_gateway(&directory, key_env, input_receiver).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        send_voice_input(&input, recording_frame(1)).await;
        let path = wait_for_recording_bytes(directory.path(), 64).await;

        // The client goes away mid-meeting: no stop, no finalize.
        gateway.close().await;

        let recorded = read_session_recording(&path, key_env).unwrap();
        assert!(
            matches!(
                recorded.chunks.last(),
                Some(conversation_runtime::RecordedChunk::Ended { .. })
            ),
            "the recording is closed, not left open, when the gateway exits"
        );
    }

    #[tokio::test]
    async fn finalize_recording_is_idempotent_and_refuses_another_session() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_IDEMPOTENT";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let (input, input_receiver) = mpsc::channel(8);
        let mut gateway = recorded_gateway(&directory, key_env, input_receiver).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));
        send_voice_input(&input, recording_frame(1)).await;
        wait_for_recording_bytes(directory.path(), 80).await;

        // A recording is closed only after the session that fed it has stopped.
        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;

        let finalize = |request_id: &str, session_id: &str| {
            format!(
                r#"{{"protocol_version":3,"type":"finalize_recording","request_id":"{request_id}","session_id":"{session_id}"}}"#
            )
        };
        gateway.write(&finalize("fin-1", "1")).await;
        assert_accepted_message(&gateway.read_message().await, "fin-1");
        let first = gateway.read_message().await;

        gateway.write(&finalize("fin-2", "1")).await;
        assert_accepted_message(&gateway.read_message().await, "fin-2");
        let second = gateway.read_message().await;
        assert_eq!(
            first.replace("fin-1", "fin"),
            second.replace("fin-2", "fin"),
            "a repeated finalize restates the same recording"
        );

        gateway.write(&finalize("fin-3", "9")).await;
        assert_rejected_message(&gateway.read_message().await, "fin-3", "invalid_state");

        // The session is unharmed by the rejection.
        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"after"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "after");
        assert!(gateway.read_message().await.contains(r#""type":"status""#));

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn a_voice_session_without_its_recording_key_is_refused_request_scoped() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_MISSING";
        std::env::remove_var(key_env);
        let directory = TempDir::new().unwrap();
        let (_input, input_receiver) = mpsc::channel(8);
        let mut gateway = recorded_gateway(&directory, key_env, input_receiver).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "voice-1", "configuration_invalid");
        assert!(
            !rejection.contains(key_env),
            "the client is told nothing about the key variable: {rejection}"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);

        // The gateway session survives a refused recording.
        gateway
            .write(r#"{"protocol_version":3,"type":"status","request_id":"after"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "after");
        assert!(gateway.read_message().await.contains(r#""type":"status""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn finalize_recording_is_refused_without_a_recording_and_below_version_three() {
        let (session, _guards) = session_with_voice();
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
            )
            .await;
        assert_rejected_message(&gateway.read_message().await, "fin-1", "invalid_state");

        for version in [1, 2] {
            gateway
                .write(&format!(
                    r#"{{"protocol_version":{version},"type":"finalize_recording","request_id":"fin-old","session_id":"1"}}"#
                ))
                .await;
            assert_rejected_message(
                &gateway.read_message().await,
                "invalid-command",
                "invalid_state",
            );
        }

        gateway.close().await;
    }

    // --- post-meeting reconciliation -------------------------------------------------------

    #[tokio::test]
    async fn restart_protocol_is_pinned_without_changing_default_legacy_floor_policy() {
        for explicit in [false, true] {
            let session = GatewaySession::new(unused_runtime(), status());
            let mut gateway = InMemoryGateway::start(if explicit {
                session.with_v4_control()
            } else {
                session
            })
            .await;
            let refused = if explicit { 3 } else { 4 };
            gateway
                .write(&format!(
                    r#"{{"protocol_version":{refused},"type":"status","request_id":"wrong"}}"#
                ))
                .await;
            let rejection: serde_json::Value =
                serde_json::from_str(&gateway.read_message().await).unwrap();
            assert_eq!(rejection["type"], "command_rejected");
            assert_eq!(rejection["protocol_version"], if explicit { 4 } else { 3 });
            let version = if explicit { 4 } else { 1 };
            gateway
                .write(&format!(
                    r#"{{"protocol_version":{version},"type":"status","request_id":"correct"}}"#
                ))
                .await;
            let accepted: serde_json::Value =
                serde_json::from_str(&gateway.read_message().await).unwrap();
            assert_eq!(accepted["type"], "command_accepted");
            // This fixture advertises context seeding, whose legacy response floor is v2.
            assert_eq!(accepted["protocol_version"], if explicit { 4 } else { 2 });
            gateway.read_message().await;
            gateway.close().await;
        }
    }

    #[tokio::test]
    async fn restart_recovery_refuses_live_voice_and_pending_text_before_authorization() {
        let command = serde_json::json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"recovery","capability":{
            "source_session_id":"41","recording_handle":"a".repeat(32),"sha256":"b".repeat(64),"key_id":"c".repeat(16),"authorization":"d".repeat(64)
        }}).to_string();
        let (input, receiver) = mpsc::channel(8);
        let session = session_with_interactive_voice(
            receiver,
            Arc::new(MockGenerationLanguageModel::new(["reply"])),
            no_speech_frames(),
        )
        .with_v4_control();
        let mut voice = InMemoryGateway::start(session).await;
        voice
            .write(r#"{"protocol_version":4,"type":"start_voice_session","request_id":"voice"}"#)
            .await;
        voice
            .read_until(|value| value.contains("voice_session_started"))
            .await;
        voice.write(&command).await;
        let rejected = voice.read_message().await;
        assert!(rejected.contains("recording recovery requires an idle session"));
        drop(input);
        voice.close().await;

        let started = Arc::new(Notify::new());
        let model = Arc::new(PendingLanguageModel {
            started: started.clone(),
            release: Arc::new(Notify::new()),
        });
        let session = GatewaySession::new(
            conversation_runtime::TextTurnRuntime::new(test_context(), model),
            status(),
        )
        .with_v4_control();
        let mut text = InMemoryGateway::start(session).await;
        text.write(r#"{"protocol_version":4,"type":"start_turn","request_id":"turn","transcript":"hello"}"#).await;
        timeout(TEST_TIMEOUT, started.notified()).await.unwrap();
        text.write(&command).await;
        let rejected = text
            .read_until(|value| value.contains(r#""request_id":"recovery""#))
            .await;
        assert!(rejected
            .last()
            .unwrap()
            .contains("recording recovery requires an idle session"));
        text.close().await;
    }

    #[tokio::test]
    async fn enriched_reconciliation_over_actual_wire_budget_is_rejected_before_acceptance() {
        struct EnrichedDiarizer;
        impl TranscriptionDiarizer for EnrichedDiarizer {
            fn diarize<'a>(
                &'a self,
                mut windows: MeetingAudioWindows,
                _cancellation: CancellationToken,
            ) -> AdapterFuture<'a, DiarizationResult> {
                Box::pin(async move {
                    while let Some(window) = windows.recv().await {
                        window?;
                    }
                    let range = conversation_model_adapters::DiarizedMediaSampleRange::new(
                        23, 25, 24_000, 0,
                    )
                    .unwrap();
                    let segments = (0..1024)
                        .map(|index| {
                            DiarizedSegment::new(
                                format!("seg-{index:060}"),
                                0,
                                1,
                                "\"".repeat(128),
                                Some("cluster-1".to_owned()),
                            )?
                            .with_media_sample_range(range)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    DiarizationResult::new(segments, Vec::new())
                })
            }
        }
        let directory = TempDir::new().unwrap();
        let key_env = "RUNTIME_DIARIZATION_ENRICHED_BOUND_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
        let mut recorder =
            conversation_runtime::SessionRecorder::start(&settings, SessionId::new(41)).unwrap();
        let (_, capability) = recorder.finalize_with_capability().unwrap();
        drop(recorder);
        let session = GatewaySession::new(unused_runtime(), status())
            .with_v4_control()
            .with_recording(settings)
            .with_reconciliation(Arc::new(EnrichedDiarizer));
        let mut gateway = InMemoryGateway::start(session).await;
        gateway.write(&serde_json::json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"enriched-bound","capability":{
            "source_session_id":"41","recording_handle":capability.recording_handle,"sha256":capability.sha256,"key_id":capability.key_id,"authorization":capability.authorization
        }}).to_string()).await;
        let response = gateway.read_message().await;
        if response.contains("command_rejected") {
            gateway
                .write(r#"{"protocol_version":4,"type":"status","request_id":"still-open"}"#)
                .await;
            assert_accepted_message(&gateway.read_message().await, "still-open");
            assert!(gateway.read_message().await.contains(r#""type":"status""#));
        }
        let InMemoryGateway {
            input,
            output,
            task,
        } = gateway;
        drop(input);
        drop(output);
        let exited = timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
        assert_rejected_message(&response, "enriched-bound", "invalid_state");
        assert!(response.len() < 1024 && !response.contains(FAKE_RECORDING_KEY));
        assert!(exited.is_ok(), "bounded rejection leaves gateway healthy");
    }

    #[tokio::test]
    async fn restart_recovery_on_control_eof_cancels_and_joins_provider_work() {
        struct PendingRecovery {
            started: Arc<Notify>,
            finished: Arc<Notify>,
        }
        impl TranscriptionDiarizer for PendingRecovery {
            fn diarize<'a>(
                &'a self,
                windows: MeetingAudioWindows,
                cancellation: CancellationToken,
            ) -> AdapterFuture<'a, DiarizationResult> {
                Box::pin(async move {
                    self.started.notify_one();
                    cancellation.cancelled().await;
                    drop(windows);
                    self.finished.notify_one();
                    Err(AdapterError::new("synthetic cancellation"))
                })
            }
        }
        let directory = TempDir::new().unwrap();
        let key_env = "RUNTIME_GATEWAY_RECOVERY_CANCEL_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
        let mut recorder =
            conversation_runtime::SessionRecorder::start(&settings, SessionId::new(41)).unwrap();
        let (_, capability) = recorder.finalize_with_capability().unwrap();
        let closed = CancellationToken::new();
        let started = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let session = GatewaySession::new(unused_runtime(), status())
            .with_v4_control()
            .with_control_closed(closed.clone())
            .with_recording(settings)
            .with_reconciliation(Arc::new(PendingRecovery {
                started: started.clone(),
                finished: finished.clone(),
            }));
        let mut gateway = InMemoryGateway::start(session).await;
        let command = serde_json::json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"recover","capability":{
            "source_session_id":capability.source_session_id.get().to_string(),"recording_handle":capability.recording_handle,"sha256":capability.sha256,"key_id":capability.key_id,"authorization":capability.authorization
        }});
        gateway.write(&command.to_string()).await;
        timeout(TEST_TIMEOUT, started.notified()).await.unwrap();
        closed.cancel();
        timeout(TEST_TIMEOUT, finished.notified()).await.unwrap();
        let response = gateway.read_message().await;
        assert_rejected_message(&response, "recover", "invalid_state");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        gateway.close().await;
        std::env::remove_var(key_env);
    }

    /// A local stand-in for the online diarizer. It answers with one clustered segment and one
    /// unclustered one, which is enough to state the whole wire shape.
    struct FixtureDiarizer;

    struct EmptyDiarizer;

    impl TranscriptionDiarizer for EmptyDiarizer {
        fn diarize<'a>(
            &'a self,
            mut windows: MeetingAudioWindows,
            _cancellation: CancellationToken,
        ) -> AdapterFuture<'a, DiarizationResult> {
            Box::pin(async move {
                while let Some(window) = windows.recv().await {
                    let _ = window?;
                }
                DiarizationResult::new(Vec::new(), Vec::new())
            })
        }
    }

    impl TranscriptionDiarizer for FixtureDiarizer {
        fn diarize<'a>(
            &'a self,
            mut windows: MeetingAudioWindows,
            _cancellation: CancellationToken,
        ) -> AdapterFuture<'a, DiarizationResult> {
            Box::pin(async move {
                let mut audio_bytes = 0_usize;
                while let Some(window) = windows.recv().await {
                    audio_bytes += window?.bytes().len();
                }
                assert!(audio_bytes > 0, "the reconciliation must be handed audio");
                DiarizationResult::new(
                    vec![
                        DiarizedSegment::new(
                            "seg-1",
                            0,
                            900,
                            "good morning",
                            Some("cluster-1".to_owned()),
                        )?,
                        DiarizedSegment::new("seg-2", 900, 1_400, "morning", None)?,
                    ],
                    Vec::new(),
                )
            })
        }
    }

    /// The batch text-processing call is answered inline on the control channel, so it must not
    /// run while a voice session could need that channel for an interrupt. This test lives here
    /// rather than in `tests/text_processing.rs` because `GatewayVoiceAdapters` is crate-private.
    #[tokio::test]
    async fn process_text_is_refused_while_a_voice_session_is_active() {
        let (input, input_receiver) = mpsc::channel(8);
        let processing = Arc::new(MockGenerationLanguageModel::new([
            r#"{"decisions":[]}"#.to_owned()
        ]));
        let session = session_with_voice_and_runtime(
            conversation_runtime::TextTurnRuntime::new(test_context(), processing.clone()),
            input_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        );
        let mut gateway = InMemoryGateway::start(session).await;
        let command = |request_id: &str| {
            format!(
                r#"{{"protocol_version":3,"type":"process_text","request_id":"{request_id}","session_id":"1","instructions":"summarise the meeting","input":"we agreed to ship","output_schema":{{"type":"object"}},"max_output_bytes":4096}}"#
            )
        };

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        gateway.write(&command("pt-voice")).await;
        let refused = gateway
            .read_until(|message| message.contains(r#""request_id":"pt-voice""#))
            .await;
        let rejection = refused
            .last()
            .expect("read_until returns the frame it matched");
        assert_rejected_message(rejection, "pt-voice", "invalid_state");
        assert!(
            rejection.contains("text processing cannot run while a voice session is active"),
            "the rejection names its own cause: {rejection}"
        );
        assert!(
            processing.requests().is_empty(),
            "a refused call never reaches the language model"
        );

        // A state guard, not a refusal: once the session has stopped the same call is served.
        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;
        gateway.write(&command("pt-after")).await;
        let served = gateway
            .read_until(|message| message.contains(r#""type":"text_processed""#))
            .await;
        let processed: serde_json::Value =
            serde_json::from_str(served.last().expect("read_until returns its match")).unwrap();
        assert_eq!(processed["request_id"], "pt-after");
        assert_eq!(
            processed["processed"],
            serde_json::json!({"output": {"decisions": []}, "warnings": []})
        );
        assert_eq!(processing.requests().len(), 1);

        drop(input);
    }

    #[tokio::test]
    async fn reconcile_recording_answers_with_anonymous_clusters_over_the_control_channel() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_RECONCILE";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let (input, input_receiver) = mpsc::channel(8);
        let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
        let session = session_with_interactive_voice(
            input_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        )
        .with_recording(settings)
        .with_reconciliation(Arc::new(FixtureDiarizer));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));
        send_voice_input(&input, recording_frame(1)).await;
        send_voice_input(&input, recording_frame(2)).await;
        wait_for_recording_bytes(directory.path(), 128).await;

        // Reconciling under the running media path would answer for a prefix of the meeting.
        gateway
            .write(
                r#"{"protocol_version":3,"type":"reconcile_recording","request_id":"early","session_id":"1","recording_handle":"0123456789abcdef0123456789abcdef"}"#,
            )
            .await;
        let early = gateway
            .read_until(|message| message.contains(r#""request_id":"early""#))
            .await;
        let rejection = early
            .last()
            .expect("read_until returns the frame it matched");
        assert_rejected_message(rejection, "early", "invalid_state");
        assert!(
            rejection.contains("the voice session is still recording"),
            "the rejection names its own cause: {rejection}"
        );

        gateway
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        gateway
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;
        gateway
            .write(
                r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"1"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "fin-1");
        let finalized: serde_json::Value =
            serde_json::from_str(&gateway.read_message().await).unwrap();
        let handle = finalized["recording"]["recording_handle"]
            .as_str()
            .unwrap()
            .to_owned();

        gateway
            .write(&format!(
                r#"{{"protocol_version":3,"type":"reconcile_recording","request_id":"rec-1","session_id":"1","recording_handle":"{handle}"}}"#
            ))
            .await;
        assert_accepted_message(&gateway.read_message().await, "rec-1");
        let reconciled = gateway.read_message().await;
        assert!(reconciled.contains(r#""type":"recording_reconciled""#));
        assert!(
            !reconciled.contains(FAKE_RECORDING_KEY) && !reconciled.contains(key_env),
            "the control channel carries neither the key nor its variable: {reconciled}"
        );

        let value: serde_json::Value = serde_json::from_str(&reconciled).unwrap();
        assert_eq!(
            value["reconciliation"],
            serde_json::json!({
                "segments": [
                    {
                        "segment_id": "seg-1",
                        "started_at_ms": "0",
                        "ended_at_ms": "900",
                        "text": "good morning",
                        "speaker_cluster": "cluster-1",
                    },
                    {
                        "segment_id": "seg-2",
                        "started_at_ms": "900",
                        "ended_at_ms": "1400",
                        "text": "morning",
                        "speaker_cluster": null,
                    },
                ],
                "warnings": [],
            }),
            "timings travel as decimal strings and clusters stay anonymous"
        );

        // A handle this deployment never minted is refused, whatever session claims it.
        gateway
            .write(
                r#"{"protocol_version":3,"type":"reconcile_recording","request_id":"rec-2","session_id":"1","recording_handle":"ffffffffffffffffffffffffffffffff"}"#,
            )
            .await;
        let refused = gateway.read_message().await;
        assert!(
            refused.contains(r#""request_id":"rec-2""#) && refused.contains("command_rejected"),
            "an unminted handle is refused request-scoped: {refused}"
        );

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn two_gateway_sessions_sharing_a_key_reject_a_recording_handle_from_the_other_session() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_CROSS_SESSION";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let first_session_id = SessionId::new(41);
        let second_session_id = SessionId::new(42);
        let (first_input, first_receiver) = mpsc::channel(8);
        let first = session_with_interactive_voice(
            first_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        )
        .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
        .with_reconciliation(Arc::new(FixtureDiarizer))
        .with_test_session_id(first_session_id);
        let mut first = InMemoryGateway::start(first).await;

        first
            .write(r#"{"protocol_version":3,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&first.read_message().await, "voice-1");
        let started = first.read_message().await;
        assert!(started.contains(r#""session_id":"41""#), "{started}");
        send_voice_input(&first_input, recording_frame(1)).await;
        wait_for_recording_bytes(directory.path(), 80).await;
        first
            .write(r#"{"protocol_version":3,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        first
            .read_until(|message| message.contains(r#""request_id":"stop-1""#))
            .await;
        first
            .write(r#"{"protocol_version":3,"type":"finalize_recording","request_id":"fin-1","session_id":"41"}"#)
            .await;
        assert_accepted_message(&first.read_message().await, "fin-1");
        let finalized: serde_json::Value =
            serde_json::from_str(&first.read_message().await).unwrap();
        let handle = finalized["recording"]["recording_handle"]
            .as_str()
            .unwrap()
            .to_owned();

        let (_second_input, second_receiver) = mpsc::channel(8);
        let second = session_with_interactive_voice(
            second_receiver,
            Arc::new(MockGenerationLanguageModel::new(["fixture reply"])),
            no_speech_frames(),
        )
        .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
        .with_reconciliation(Arc::new(EmptyDiarizer))
        .with_test_session_id(second_session_id);
        let mut second = InMemoryGateway::start(second).await;
        second
            .write(&format!(
                r#"{{"protocol_version":3,"type":"reconcile_recording","request_id":"cross","session_id":"42","recording_handle":"{handle}"}}"#
            ))
            .await;
        let rejected = second.read_message().await;
        assert_rejected_message(&rejected, "cross", "invalid_state");
        assert!(
            rejected.contains("runtime state rejected the operation"),
            "the header session boundary rejects a known handle without exposing recording detail: {rejected}"
        );

        first.close().await;
        second.close().await;
    }

    #[tokio::test]
    async fn reconcile_recording_is_refused_without_an_adapter_and_below_version_three() {
        let key_env = "CONVERSATION_SESSION_RECORDING_KEY_NO_ADAPTER";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        let directory = TempDir::new().unwrap();
        let handle = "0123456789abcdef0123456789abcdef";
        let command = |request_id: &str, version: u64| {
            format!(
                r#"{{"protocol_version":{version},"type":"reconcile_recording","request_id":"{request_id}","session_id":"1","recording_handle":"{handle}"}}"#
            )
        };

        // No `[recording]` section at all: the deployment records nothing to reconcile.
        let (session, _guards) = session_with_voice();
        let mut gateway = InMemoryGateway::start(session).await;
        gateway.write(&command("rec-1", 3)).await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "rec-1", "invalid_state");
        assert!(rejection.contains("this deployment does not record sessions"));

        for version in [1, 2] {
            gateway.write(&command("rec-old", version)).await;
            assert_rejected_message(
                &gateway.read_message().await,
                "invalid-command",
                "invalid_state",
            );
        }
        // A handle that could name a path outside the recording directory never decodes.
        gateway
            .write(
                r#"{"protocol_version":3,"type":"reconcile_recording","request_id":"rec-2","session_id":"1","recording_handle":"../escape"}"#,
            )
            .await;
        assert_rejected_message(
            &gateway.read_message().await,
            "invalid-command",
            "invalid_state",
        );
        gateway.close().await;

        // Recording configured, but no diarizer wired: a different cause, stated as such.
        let (_input, input_receiver) = mpsc::channel(8);
        let mut recorded = recorded_gateway(&directory, key_env, input_receiver).await;
        recorded.write(&command("rec-3", 3)).await;
        let rejection = recorded.read_message().await;
        assert_rejected_message(&rejection, "rec-3", "invalid_state");
        assert!(
            rejection.contains("this deployment does not reconcile recordings"),
            "the two causes must be distinguishable: {rejection}"
        );
        recorded.close().await;
    }

    /// Both timed starts share the one start pipeline, but each binds exactly
    /// the media version it asked for: the old command stays media2 and the
    /// additive drainable command yields a media3 lease. Unknown command types
    /// keep the generic decode rejection with no fallback.
    #[tokio::test]
    async fn timed_starts_bind_the_exact_requested_media_version_and_unknown_commands_stay_rejected(
    ) {
        let key_env = "RUNTIME_DRAINABLE_ADMISSION_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        for (command, expected_version) in [
            ("start_timed_voice_session", 2),
            ("start_drainable_timed_voice_session", 3),
        ] {
            let directory = TempDir::new().unwrap();
            let (session, _guards) = session_with_voice();
            let media = Arc::new(
                MediaServer::bind(
                    MediaServerConfig::loopback()
                        .with_capture_format(Some("pcm_s16le/24000/1/20ms".into()))
                        .unwrap(),
                )
                .await
                .unwrap(),
            );
            let session = session
                .with_v4_control()
                .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
                .with_media_server(Arc::clone(&media));
            let mut gateway = InMemoryGateway::start(session).await;
            gateway.write(&format!(r#"{{"protocol_version":4,"type":"{command}","request_id":"timed","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}}"#)).await;
            assert_accepted_message(&gateway.read_message().await, "timed");
            let started: serde_json::Value =
                serde_json::from_str(&gateway.read_message().await).unwrap();
            assert_eq!(started["type"], "timed_voice_session_started", "{command}");
            assert_eq!(started["request_id"], "timed");
            assert_eq!(
                started["media_lease"]["media_protocol_version"], expected_version,
                "{command} bound the wrong media version"
            );
            assert_eq!(
                started["media_lease"]["capture_format"],
                "pcm_s16le/24000/1/20ms"
            );
            gateway.close().await;
            media.shutdown().await;
        }
        let (session, _guards) = session_with_voice();
        let mut gateway = InMemoryGateway::start(session.with_v4_control()).await;
        gateway.write(r#"{"protocol_version":4,"type":"start_drainable_timed_voice_session_v9","request_id":"unknown","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}"#).await;
        assert_rejected_message(
            &gateway.read_message().await,
            "invalid-command",
            "invalid_state",
        );
        gateway.close().await;
        std::env::remove_var(key_env);
    }

    /// A writer that completes every write except the one carrying `marker`, which stays
    /// pending until released. While it is held the writer loop has already picked the
    /// marked frame's lane, so the lane priority for its next pick is fixed.
    struct MarkerBlockingWriter {
        marker: &'static [u8],
        state: Arc<MarkerBlockingWriterState>,
    }

    struct MarkerBlockingWriterState {
        bytes: StdMutex<Vec<u8>>,
        blocked: Condition,
        released: AtomicBool,
        waker: StdMutex<Option<Waker>>,
    }

    impl MarkerBlockingWriter {
        fn new(marker: &'static str) -> (Self, Arc<MarkerBlockingWriterState>) {
            let state = Arc::new(MarkerBlockingWriterState {
                bytes: StdMutex::new(Vec::new()),
                blocked: Condition::default(),
                released: AtomicBool::new(false),
                waker: StdMutex::new(None),
            });
            (
                Self {
                    marker: marker.as_bytes(),
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl AsyncWrite for MarkerBlockingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let marked = bytes
                .windows(self.marker.len())
                .any(|window| window == self.marker);
            if marked && !self.state.released.load(Ordering::SeqCst) {
                *self.state.waker.lock().unwrap() = Some(context.waker().clone());
                self.state.blocked.set();
                if !self.state.released.load(Ordering::SeqCst) {
                    return Poll::Pending;
                }
            }
            self.state.bytes.lock().unwrap().extend_from_slice(bytes);
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

    impl MarkerBlockingWriterState {
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }
    }

    /// Wire order seen in the failing D2 runs: a `status` reply (normal lane) was the
    /// last frame written, so the writer next prefers the event lane. If the timed
    /// start's acceptance and the session's `voice_session_started` are both queued
    /// before the writer picks again, the acceptance must still be written first:
    /// the SDK only opens its voice session on that acceptance.
    #[tokio::test]
    async fn timed_start_acceptance_is_written_before_its_first_voice_event_after_a_normal_lane_reply(
    ) {
        let key_env = "RUNTIME_TIMED_START_ORDER_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        for command in [
            "start_timed_voice_session",
            "start_drainable_timed_voice_session",
        ] {
            let directory = TempDir::new().unwrap();
            let (session, _guards) = session_with_voice();
            let media = Arc::new(
                MediaServer::bind(
                    MediaServerConfig::loopback()
                        .with_capture_format(Some("pcm_s16le/24000/1/20ms".into()))
                        .unwrap(),
                )
                .await
                .unwrap(),
            );
            let session = session
                .with_v4_control()
                .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap())
                .with_media_server(Arc::clone(&media));
            let (mut client_input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
            let (writer, writer_state) = MarkerBlockingWriter::new(r#""type":"status""#);
            let writer_lanes = super::WriterLanes::new();
            let urgent_monitor = writer_lanes.urgent_sender.clone();
            let normal_monitor = writer_lanes.normal_sender.clone();
            let event_monitor = writer_lanes.event_sender.clone();
            let session_task =
                tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

            write_command(
                &mut client_input,
                r#"{"protocol_version":4,"type":"status","request_id":"status-1"}"#,
            )
            .await;
            timeout(TEST_TIMEOUT, writer_state.blocked.wait())
                .await
                .expect("gateway writer did not block on the status reply");
            write_command(
                &mut client_input,
                &format!(r#"{{"protocol_version":4,"type":"{command}","request_id":"timed","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}}"#),
            )
            .await;
            timeout(TEST_TIMEOUT, async {
                while queued_messages(&urgent_monitor) + queued_messages(&normal_monitor) < 2
                    || queued_messages(&event_monitor) < 1
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("timed start replies and voice_session_started were not all queued");

            drop(urgent_monitor);
            drop(normal_monitor);
            drop(event_monitor);
            writer_state.release();
            drop(client_input);
            timeout(TEST_TIMEOUT, session_task)
                .await
                .expect("gateway session did not close")
                .unwrap()
                .unwrap();
            media.shutdown().await;

            let messages = decode_frames(&writer_state.bytes());
            let types = messages
                .iter()
                .map(|message| message_type(message).to_owned())
                .collect::<Vec<_>>();
            let accepted = message_index(&messages, |message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"timed""#)
            });
            let timed = message_index(&messages, |message| {
                message_type(message) == "timed_voice_session_started"
            });
            let voice_started = message_index(&messages, |message| {
                message.contains(r#""type":"voice_session_started""#)
            });
            assert!(
                accepted < voice_started,
                "{command}: voice_session_started was written before the start's command_accepted: {types:?}"
            );
            assert!(accepted < timed, "{command}: {types:?}");
        }
        std::env::remove_var(key_env);
    }

    #[tokio::test]
    async fn timed_start_requires_recording_network_media_and_explicit_format_before_acceptance() {
        let key_env = "RUNTIME_TIMED_ADMISSION_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_RECORDING_KEY);
        for missing in ["recording", "media", "format"] {
            let directory = TempDir::new().unwrap();
            let (mut session, guards) = session_with_voice();
            session = session.with_v4_control();
            if missing != "recording" {
                session = session
                    .with_recording(RecordingSettings::new(directory.path(), key_env).unwrap());
            }
            let media = if missing != "media" {
                let config = MediaServerConfig::loopback()
                    .with_capture_format(if missing == "format" {
                        None
                    } else {
                        Some("pcm_s16le/24000/1/20ms".into())
                    })
                    .unwrap();
                Some(Arc::new(MediaServer::bind(config).await.unwrap()))
            } else {
                None
            };
            if let Some(media) = &media {
                session = session.with_media_server(media.clone());
            }
            let mut gateway = InMemoryGateway::start(session).await;
            gateway.write(r#"{"protocol_version":4,"type":"start_timed_voice_session","request_id":"timed","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}"#).await;
            assert_rejected_message(&gateway.read_message().await, "timed", "invalid_state");
            assert_eq!(
                guards.io_factory.start_count(),
                0,
                "missing {missing} started capture"
            );
            assert!(std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none());
            gateway.close().await;
            if let Some(media) = media {
                media.shutdown().await;
            }
        }
        std::env::remove_var(key_env);
    }
}
