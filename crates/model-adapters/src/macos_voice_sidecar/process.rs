use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use command_fds::{CommandFdExt, FdMapping};
use conversation_protocol::{
    GenerationId, PlaybackState, RuntimeStage, SessionId, TurnId, UtteranceId,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::codec::{
    decode_frame, decode_frame_at_eof, encode_frame, SidecarControl, SidecarFailureCode,
    SidecarFrame, MAX_CONTROL_PAYLOAD_BYTES, PROTOCOL_VERSION,
};
use crate::{
    AdapterError, AdapterFuture, AudioFrame, ContinuousAudioOutput, PlaybackReceipt,
    RecognitionEvent, VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};

const SPEECH_START_MS: std::ops::RangeInclusive<u64> = 100..=1_000;
const FINAL_SILENCE_MS: std::ops::RangeInclusive<u64> = 200..=3_000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const TASK_CLEANUP_TIMEOUT: Duration = Duration::from_millis(100);
const CONTROL_QUEUE_CAPACITY: usize = 16;
const MEDIA_QUEUE_CAPACITY: usize = 100;
const INPUT_QUEUE_CAPACITY: usize = 32;
const RELIABLE_INPUT_QUEUE_CAPACITY: usize = 16;
const NONTERMINAL_INPUT_QUEUE_CAPACITY: usize = 1;
const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_QUEUED_MEDIA_NANOS: u128 = 2_000_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SystemDevice {
    SystemDefault,
    Named(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacOsVoiceSidecarConfig {
    executable: PathBuf,
    model_path: PathBuf,
    device: SystemDevice,
    speech_start_ms: u64,
    final_silence_ms: u64,
    max_payload_bytes: usize,
    max_stderr_bytes: usize,
}

impl MacOsVoiceSidecarConfig {
    pub fn new(
        executable: impl AsRef<Path>,
        model_path: impl AsRef<Path>,
        device: SystemDevice,
        download: bool,
        speech_start_ms: u64,
        final_silence_ms: u64,
    ) -> Result<Self, AdapterError> {
        let executable = executable.as_ref();
        let model_path = model_path.as_ref();
        require_absolute_path(executable, "sidecar executable")?;
        require_executable_file(executable)?;
        require_absolute_path(model_path, "ASR model path")?;
        if device != SystemDevice::SystemDefault {
            return Err(configuration_error(
                "only the system-default audio device is supported",
            ));
        }
        if download {
            return Err(configuration_error(
                "local ASR model download must be disabled",
            ));
        }
        if !SPEECH_START_MS.contains(&speech_start_ms) {
            return Err(configuration_error(
                "speech start threshold is outside the supported range",
            ));
        }
        if !FINAL_SILENCE_MS.contains(&final_silence_ms) {
            return Err(configuration_error(
                "final silence threshold is outside the supported range",
            ));
        }

        Ok(Self {
            executable: executable.to_path_buf(),
            model_path: model_path.to_path_buf(),
            device,
            speech_start_ms,
            final_silence_ms,
            max_payload_bytes: MAX_CONTROL_PAYLOAD_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
        })
    }

    pub fn with_max_payload_bytes(
        mut self,
        max_payload_bytes: usize,
    ) -> Result<Self, AdapterError> {
        if max_payload_bytes == 0 {
            return Err(configuration_error("payload byte limit must be non-zero"));
        }
        if max_payload_bytes > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(configuration_error(
                "payload byte limit exceeded the protocol maximum",
            ));
        }
        self.max_payload_bytes = max_payload_bytes;
        Ok(self)
    }

    pub fn with_max_stderr_bytes(mut self, max_stderr_bytes: usize) -> Result<Self, AdapterError> {
        if max_stderr_bytes == 0 {
            return Err(configuration_error("stderr byte limit must be non-zero"));
        }
        self.max_stderr_bytes = max_stderr_bytes;
        Ok(self)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub const fn device(&self) -> &SystemDevice {
        &self.device
    }

    pub const fn speech_start_ms(&self) -> u64 {
        self.speech_start_ms
    }

    pub const fn final_silence_ms(&self) -> u64 {
        self.final_silence_ms
    }

    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    pub const fn max_stderr_bytes(&self) -> usize {
        self.max_stderr_bytes
    }
}

#[derive(Clone, Debug)]
pub struct MacOsVoiceSidecar {
    config: MacOsVoiceSidecarConfig,
}

impl MacOsVoiceSidecar {
    pub const fn new(config: MacOsVoiceSidecarConfig) -> Self {
        Self { config }
    }
}

pub type MacOsVoiceSidecarSession = VoiceIoSession;

impl VoiceIoFactory for MacOsVoiceSidecar {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(start_sidecar(self.config.clone(), session_id, cancellation))
    }
}

async fn start_sidecar(
    config: MacOsVoiceSidecarConfig,
    session_id: SessionId,
    cancellation: CancellationToken,
) -> Result<VoiceIoSession, AdapterError> {
    if cancellation.is_cancelled() {
        return Err(AdapterError::new("voice sidecar session cancelled"));
    }

    let SpawnedProcess {
        mut child,
        stdin,
        stdout,
        stderr,
        media,
    } = spawn_process(&config).await?;
    let shared = Arc::new(SessionShared::new(session_id));
    let media_budget = Arc::new(MediaBudget::new());
    let frame_capacity = Arc::new(Semaphore::new(MEDIA_QUEUE_CAPACITY));
    let (control_sender, control_receiver) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    let (media_sender, media_receiver) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (input_sender, input_receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
    let (reliable_input_sender, reliable_input_receiver) =
        mpsc::channel(RELIABLE_INPUT_QUEUE_CAPACITY);
    let (nonterminal_input_sender, nonterminal_input_receiver) =
        mpsc::channel(NONTERMINAL_INPUT_QUEUE_CAPACITY);
    let (supervisor_sender, mut supervisor_receiver) = mpsc::unbounded_channel();
    let io_cancellation = CancellationToken::new();
    let media_cancellation = CancellationToken::new();
    let input_publisher = InputPublisher {
        reliable: reliable_input_sender,
        nonterminal: nonterminal_input_sender,
        supervisor: supervisor_sender.clone(),
    };

    let tasks = ProcessTasks {
        control: tokio::spawn(run_control_writer(
            stdin,
            control_receiver,
            io_cancellation.clone(),
            supervisor_sender.clone(),
        )),
        media: Some(tokio::spawn(run_media_writer(
            media,
            media_receiver,
            Arc::clone(&shared),
            media_cancellation.clone(),
            io_cancellation.clone(),
            supervisor_sender.clone(),
        ))),
        stdout: tokio::spawn(run_stdout_reader(
            stdout,
            config.max_payload_bytes(),
            session_id,
            Arc::clone(&shared),
            input_publisher.clone(),
            io_cancellation.clone(),
            supervisor_sender.clone(),
        )),
        input: tokio::spawn(run_input_dispatcher(
            reliable_input_receiver,
            nonterminal_input_receiver,
            input_sender,
            io_cancellation.clone(),
            supervisor_sender.clone(),
        )),
        stderr: tokio::spawn(run_stderr_reader(
            stderr,
            config.max_stderr_bytes(),
            supervisor_sender,
        )),
    };

    let start_session = SidecarFrame::control(SidecarControl::StartSession {
        session_id,
        speech_start_ms: config.speech_start_ms(),
        final_silence_ms: config.final_silence_ms(),
    });
    if send_control(
        &control_sender,
        start_session,
        &cancellation,
        &io_cancellation,
    )
    .await
    .is_err()
    {
        let error = AdapterError::new("failed to start voice sidecar session");
        let cleanup = cleanup_process(
            &mut child,
            false,
            tasks,
            &shared,
            &media_cancellation,
            &io_cancellation,
            error.clone(),
        )
        .await;
        return Err(cleanup.err().unwrap_or(error));
    }

    let startup = tokio::time::timeout(
        STARTUP_TIMEOUT,
        wait_for_ready(&mut child, &mut supervisor_receiver, cancellation.clone()),
    )
    .await;
    let (startup_error, child_reaped) = match startup {
        Ok(StartupOutcome::Ready) => {
            let input = Arc::new(MacOsVoiceInput {
                session_id,
                control_sender: control_sender.clone(),
                receiver: AsyncMutex::new(Some(input_receiver)),
                session_cancellation: cancellation.clone(),
                io_cancellation: io_cancellation.clone(),
            });
            let output = Arc::new(MacOsContinuousAudioOutput {
                session_id,
                control_sender: control_sender.clone(),
                media_sender,
                shared: Arc::clone(&shared),
                media_budget,
                frame_capacity,
                session_cancellation: cancellation.clone(),
            });
            let completion = tokio::spawn(
                ProcessSupervisor {
                    child,
                    tasks,
                    events: supervisor_receiver,
                    control_sender,
                    input_publisher,
                    shared: Arc::clone(&shared),
                    session_cancellation: cancellation,
                    media_cancellation,
                    io_cancellation,
                }
                .run(),
            );
            return Ok(VoiceIoSession {
                input,
                output,
                completion,
            });
        }
        Ok(StartupOutcome::Failed(error)) => (error, false),
        Ok(StartupOutcome::Exited) => (
            AdapterError::new("voice sidecar process exited before readiness"),
            true,
        ),
        Ok(StartupOutcome::WaitFailed) => (
            AdapterError::new("failed to wait for voice sidecar readiness"),
            false,
        ),
        Ok(StartupOutcome::Cancelled) => {
            (AdapterError::new("voice sidecar session cancelled"), false)
        }
        Err(_) => (AdapterError::new("voice sidecar startup timed out"), false),
    };

    let cleanup = cleanup_process(
        &mut child,
        child_reaped,
        tasks,
        &shared,
        &media_cancellation,
        &io_cancellation,
        startup_error.clone(),
    )
    .await;
    Err(cleanup.err().unwrap_or(startup_error))
}

struct SpawnedProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    media: UnixStream,
}

async fn spawn_process(config: &MacOsVoiceSidecarConfig) -> Result<SpawnedProcess, AdapterError> {
    let (parent_media, child_media) = std::os::unix::net::UnixStream::pair()
        .map_err(|_| AdapterError::new("failed to create voice sidecar media socket"))?;
    parent_media
        .set_nonblocking(true)
        .map_err(|_| AdapterError::new("failed to configure voice sidecar media socket"))?;
    let media = UnixStream::from_std(parent_media)
        .map_err(|_| AdapterError::new("failed to open voice sidecar media output"))?;

    let mut command = Command::new(config.executable());
    command
        .arg("--model-path")
        .arg(config.model_path())
        .arg("--device")
        .arg("system-default")
        .arg("--download")
        .arg("false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
        .as_std_mut()
        .fd_mappings(vec![FdMapping {
            parent_fd: child_media.into(),
            child_fd: 3,
        }])
        .map_err(|_| AdapterError::new("failed to map voice sidecar media descriptor"))?;

    let mut child = command
        .spawn()
        .map_err(|_| AdapterError::new("failed to spawn voice sidecar process"))?;
    let Some(stdin) = child.stdin.take() else {
        reap_failed_setup(&mut child).await;
        return Err(AdapterError::new(
            "failed to open voice sidecar control input",
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        reap_failed_setup(&mut child).await;
        return Err(AdapterError::new(
            "failed to open voice sidecar event output",
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        reap_failed_setup(&mut child).await;
        return Err(AdapterError::new(
            "failed to open voice sidecar error output",
        ));
    };
    Ok(SpawnedProcess {
        child,
        stdin,
        stdout,
        stderr,
        media,
    })
}

async fn reap_failed_setup(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

struct MacOsVoiceInput {
    session_id: SessionId,
    control_sender: mpsc::Sender<SidecarFrame>,
    receiver: AsyncMutex<Option<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>>,
    session_cancellation: CancellationToken,
    io_cancellation: CancellationToken,
}

impl VoiceInput for MacOsVoiceInput {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new("voice sidecar session identity mismatch"));
            }
            if cancellation.is_cancelled() || self.session_cancellation.is_cancelled() {
                return Err(AdapterError::new("voice sidecar input cancelled"));
            }

            let mut receiver = self.receiver.lock().await;
            if receiver.is_none() {
                return Err(AdapterError::new("voice sidecar input already started"));
            }
            send_control(
                &self.control_sender,
                SidecarFrame::control(SidecarControl::StartCapture { session_id }),
                &cancellation,
                &self.io_cancellation,
            )
            .await
            .map_err(|_| AdapterError::new("failed to start voice sidecar capture"))?;
            let receiver = receiver
                .take()
                .ok_or_else(|| AdapterError::new("voice sidecar input already started"))?;
            let session_cancellation = self.session_cancellation.clone();
            let io_cancellation = self.io_cancellation.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancellation.cancelled() => session_cancellation.cancel(),
                    _ = session_cancellation.cancelled() => {},
                    _ = io_cancellation.cancelled() => {}
                }
            });
            Ok(receiver)
        })
    }
}

struct MacOsContinuousAudioOutput {
    session_id: SessionId,
    control_sender: mpsc::Sender<SidecarFrame>,
    media_sender: mpsc::Sender<MediaWrite>,
    shared: Arc<SessionShared>,
    media_budget: Arc<MediaBudget>,
    frame_capacity: Arc<Semaphore>,
    session_cancellation: CancellationToken,
}

impl ContinuousAudioOutput for MacOsContinuousAudioOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("voice sidecar media enqueue cancelled"));
            }
            self.shared.validate_enqueue(frame.generation_id())?;
            let identity = MediaIdentity::from(&frame);
            let encoded = encode_frame(&SidecarFrame::audio(self.session_id, frame.clone()))
                .map_err(|_| AdapterError::new("failed to encode voice sidecar media frame"))?;
            let duration_nanos = frame_duration_nanos(&frame)?;
            let frame_permit = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(AdapterError::new("voice sidecar media enqueue cancelled"));
                }
                _ = self.session_cancellation.cancelled() => {
                    return Err(AdapterError::new("voice sidecar session cancelled"));
                }
                permit = Arc::clone(&self.frame_capacity).acquire_owned() => {
                    permit.map_err(|_| AdapterError::new("voice sidecar media queue closed"))?
                }
            };
            let reservation = self
                .media_budget
                .reserve(
                    duration_nanos,
                    frame_permit,
                    &cancellation,
                    &self.session_cancellation,
                )
                .await?;
            let (completion_sender, completion_receiver) = oneshot::channel();
            let operation_id =
                self.shared
                    .register_media(identity, completion_sender, reservation)?;
            let mut operation = MediaOperationGuard::new(Arc::clone(&self.shared), operation_id);
            let write = MediaWrite {
                operation_id,
                bytes: encoded,
            };

            let sent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => false,
                _ = self.session_cancellation.cancelled() => false,
                result = self.media_sender.send(write) => result.is_ok(),
            };
            if !sent {
                return if cancellation.is_cancelled() {
                    Err(AdapterError::new("voice sidecar media enqueue cancelled"))
                } else if self.session_cancellation.is_cancelled() {
                    Err(AdapterError::new("voice sidecar session cancelled"))
                } else {
                    Err(AdapterError::new("voice sidecar media queue closed"))
                };
            }

            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(AdapterError::new("voice sidecar media enqueue cancelled"))
                }
                _ = self.session_cancellation.cancelled() => {
                    Err(AdapterError::new("voice sidecar session cancelled"))
                }
                result = completion_receiver => {
                    result.unwrap_or_else(|_| {
                        Err(AdapterError::new("voice sidecar media acknowledgement closed"))
                    })
                }
            };
            if result.is_ok() {
                operation.disarm();
            }
            result
        })
    }

    fn flush<'a>(
        &'a self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new("voice sidecar session identity mismatch"));
            }
            if self.session_cancellation.is_cancelled() {
                return Err(AdapterError::new("voice sidecar session cancelled"));
            }
            let (operation_id, completion_receiver) = self.shared.register_flush(generation_id)?;
            let mut operation = FlushOperationGuard::new(Arc::clone(&self.shared), operation_id);
            let frame = SidecarFrame::control(SidecarControl::FlushGeneration {
                session_id,
                generation_id,
                operation_id,
            });
            if self.control_sender.send(frame).await.is_err() {
                return Err(AdapterError::new("voice sidecar control queue closed"));
            }
            let result = tokio::select! {
                biased;
                _ = self.session_cancellation.cancelled() => {
                    Err(AdapterError::new("voice sidecar session cancelled"))
                }
                result = completion_receiver => {
                    result.unwrap_or_else(|_| {
                        Err(AdapterError::new("voice sidecar flush acknowledgement closed"))
                    })
                }
            };
            if result.is_ok() {
                operation.disarm();
            }
            result
        })
    }
}

struct ProcessTasks {
    control: JoinHandle<()>,
    media: Option<JoinHandle<()>>,
    stdout: JoinHandle<()>,
    input: JoinHandle<()>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
}

impl ProcessTasks {
    async fn close_media(&mut self) {
        if let Some(task) = self.media.take() {
            finish_task(task).await;
        }
    }
}

enum StartupOutcome {
    Ready,
    Failed(AdapterError),
    Exited,
    WaitFailed,
    Cancelled,
}

async fn wait_for_ready(
    child: &mut Child,
    supervisor_receiver: &mut mpsc::UnboundedReceiver<SupervisorEvent>,
    cancellation: CancellationToken,
) -> StartupOutcome {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => StartupOutcome::Cancelled,
        event = supervisor_receiver.recv() => match event {
            Some(SupervisorEvent::Ready) => StartupOutcome::Ready,
            Some(SupervisorEvent::Fatal(error)) => StartupOutcome::Failed(error),
            Some(SupervisorEvent::Eof) | Some(SupervisorEvent::ShutdownComplete) | None => {
                StartupOutcome::Failed(AdapterError::new(
                    "voice sidecar event stream ended before readiness",
                ))
            }
        },
        result = child.wait() => match result {
            Ok(_) => StartupOutcome::Exited,
            Err(_) => StartupOutcome::WaitFailed,
        },
    }
}

struct ProcessSupervisor {
    child: Child,
    tasks: ProcessTasks,
    events: mpsc::UnboundedReceiver<SupervisorEvent>,
    control_sender: mpsc::Sender<SidecarFrame>,
    input_publisher: InputPublisher,
    shared: Arc<SessionShared>,
    session_cancellation: CancellationToken,
    media_cancellation: CancellationToken,
    io_cancellation: CancellationToken,
}

impl ProcessSupervisor {
    async fn run(mut self) -> Result<(), AdapterError> {
        let (outcome, mut child_reaped) = tokio::select! {
            biased;
            _ = self.session_cancellation.cancelled() => {
                (AdapterError::new("voice sidecar session cancelled"), false)
            }
            event = self.events.recv() => {
                let error = match event {
                    Some(SupervisorEvent::Fatal(error)) => error,
                    Some(SupervisorEvent::Eof) | None => {
                        AdapterError::new("voice sidecar event stream ended unexpectedly")
                    }
                    Some(SupervisorEvent::Ready) | Some(SupervisorEvent::ShutdownComplete) => {
                        AdapterError::new("voice sidecar lifecycle event was out of order")
                    }
                };
                (error, false)
            }
            result = self.child.wait() => {
                match result {
                    Ok(_) => (
                        AdapterError::new("voice sidecar process exited unexpectedly"),
                        true,
                    ),
                    Err(_) => (
                        AdapterError::new("failed to wait for voice sidecar process"),
                        false,
                    ),
                }
            }
        };

        if self.session_cancellation.is_cancelled() && !child_reaped {
            self.media_cancellation.cancel();
            self.tasks.close_media().await;
            child_reaped = graceful_shutdown(
                &mut self.child,
                &mut self.events,
                &self.control_sender,
                &self.shared,
            )
            .await;
        } else {
            self.input_publisher.publish_reliable(Err(outcome.clone()));
        }

        let cleanup = cleanup_process(
            &mut self.child,
            child_reaped,
            self.tasks,
            &self.shared,
            &self.media_cancellation,
            &self.io_cancellation,
            outcome.clone(),
        )
        .await;
        cleanup.and(Err(outcome))
    }
}

async fn graceful_shutdown(
    child: &mut Child,
    supervisor_receiver: &mut mpsc::UnboundedReceiver<SupervisorEvent>,
    control_sender: &mpsc::Sender<SidecarFrame>,
    shared: &Arc<SessionShared>,
) -> bool {
    let graceful = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, async {
        let mut _flush_operation = None;
        let mut _flush_completion = None;
        if let Some(generation_id) = shared.active_generation() {
            if let Ok((operation_id, completion)) = shared.register_flush(generation_id) {
                _flush_operation = Some(FlushOperationGuard::new(Arc::clone(shared), operation_id));
                _flush_completion = Some(completion);
                control_sender
                    .send(SidecarFrame::control(SidecarControl::FlushGeneration {
                        session_id: shared.session_id,
                        generation_id,
                        operation_id,
                    }))
                    .await
                    .map_err(|_| ())?;
            }
        }
        control_sender
            .send(SidecarFrame::control(SidecarControl::Shutdown {
                session_id: shared.session_id,
            }))
            .await
            .map_err(|_| ())?;

        let mut shutdown_complete = false;
        loop {
            tokio::select! {
                biased;
                status = child.wait() => return status.map(|_| true).map_err(|_| ()),
                event = supervisor_receiver.recv() => match event {
                    Some(SupervisorEvent::ShutdownComplete) => shutdown_complete = true,
                    Some(SupervisorEvent::Ready) => return Err(()),
                    Some(SupervisorEvent::Fatal(_)) | Some(SupervisorEvent::Eof) | None => {
                        if shutdown_complete {
                            continue;
                        }
                        return Err(());
                    }
                }
            }
        }
    })
    .await;
    matches!(graceful, Ok(Ok(true)))
}

async fn cleanup_process(
    child: &mut Child,
    child_reaped: bool,
    mut tasks: ProcessTasks,
    shared: &Arc<SessionShared>,
    media_cancellation: &CancellationToken,
    io_cancellation: &CancellationToken,
    failure: AdapterError,
) -> Result<(), AdapterError> {
    shared.fail(failure);
    media_cancellation.cancel();
    io_cancellation.cancel();
    tasks.close_media().await;

    let child_cleanup = if child_reaped {
        Ok(())
    } else {
        let _ = child.start_kill();
        child
            .wait()
            .await
            .map(|_| ())
            .map_err(|_| AdapterError::new("failed to reap voice sidecar process"))
    };

    finish_task(tasks.control).await;
    finish_task(tasks.stdout).await;
    finish_task(tasks.input).await;
    let stderr_cleanup = finish_stderr_task(tasks.stderr).await;
    child_cleanup.and(stderr_cleanup)
}

async fn finish_task(mut task: JoinHandle<()>) {
    if tokio::time::timeout(TASK_CLEANUP_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn finish_stderr_task(
    mut task: JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<(), AdapterError> {
    let result = match tokio::time::timeout(TASK_CLEANUP_TIMEOUT, &mut task).await {
        Ok(result) => result,
        Err(_) => {
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => return Ok(()),
                result => result,
            }
        }
    };
    result
        .map_err(|_| AdapterError::new("failed to finish voice sidecar stderr capture"))?
        .map_err(|_| AdapterError::new("failed to read voice sidecar stderr"))
        .map(|_| ())
}

async fn send_control(
    sender: &mpsc::Sender<SidecarFrame>,
    frame: SidecarFrame,
    cancellation: &CancellationToken,
    io_cancellation: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(()),
        _ = io_cancellation.cancelled() => Err(()),
        result = sender.send(frame) => result.map_err(|_| ()),
    }
}

type InputDelivery = Result<VoiceInputEvent, AdapterError>;

#[derive(Clone)]
struct InputPublisher {
    reliable: mpsc::Sender<InputDelivery>,
    nonterminal: mpsc::Sender<InputDelivery>,
    supervisor: mpsc::UnboundedSender<SupervisorEvent>,
}

impl InputPublisher {
    fn publish_reliable(&self, event: InputDelivery) -> bool {
        match self.reliable.try_send(event) {
            Ok(()) => true,
            Err(_) => {
                send_fatal(
                    &self.supervisor,
                    "voice sidecar reliable input delivery overflowed",
                );
                false
            }
        }
    }

    fn publish_nonterminal(&self, event: InputDelivery) {
        let _ = self.nonterminal.try_send(event);
    }
}

async fn run_input_dispatcher(
    mut reliable: mpsc::Receiver<InputDelivery>,
    mut nonterminal: mpsc::Receiver<InputDelivery>,
    output: mpsc::Sender<InputDelivery>,
    cancellation: CancellationToken,
    supervisor_sender: mpsc::UnboundedSender<SupervisorEvent>,
) {
    let mut reliable_open = true;
    let mut nonterminal_open = true;
    while reliable_open || nonterminal_open {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            event = reliable.recv(), if reliable_open => match event {
                Some(event) => event,
                None => {
                    reliable_open = false;
                    continue;
                }
            },
            event = nonterminal.recv(), if nonterminal_open => match event {
                Some(event) => event,
                None => {
                    nonterminal_open = false;
                    continue;
                }
            },
        };
        let sent = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = output.send(event) => result,
        };
        if sent.is_err() {
            send_fatal(&supervisor_sender, "voice sidecar input consumer closed");
            return;
        }
    }
}

async fn run_control_writer(
    mut stdin: ChildStdin,
    mut receiver: mpsc::Receiver<SidecarFrame>,
    cancellation: CancellationToken,
    supervisor_sender: mpsc::UnboundedSender<SupervisorEvent>,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            frame = receiver.recv() => match frame {
                Some(frame) => frame,
                None => return,
            }
        };
        let bytes = match encode_frame(&frame) {
            Ok(bytes) => bytes,
            Err(_) => {
                send_fatal(
                    &supervisor_sender,
                    "failed to encode voice sidecar control frame",
                );
                return;
            }
        };
        let write = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = stdin.write_all(&bytes) => result,
        };
        if write.is_err() {
            send_fatal(&supervisor_sender, "failed to write voice sidecar control");
            return;
        }
    }
}

async fn run_media_writer(
    mut media: UnixStream,
    mut receiver: mpsc::Receiver<MediaWrite>,
    shared: Arc<SessionShared>,
    media_cancellation: CancellationToken,
    io_cancellation: CancellationToken,
    supervisor_sender: mpsc::UnboundedSender<SupervisorEvent>,
) {
    loop {
        let write = tokio::select! {
            biased;
            _ = media_cancellation.cancelled() => return,
            _ = io_cancellation.cancelled() => return,
            write = receiver.recv() => match write {
                Some(write) => write,
                None => return,
            }
        };
        if !shared.begin_media_write(write.operation_id) {
            continue;
        }
        let result = tokio::select! {
            biased;
            _ = media_cancellation.cancelled() => return,
            _ = io_cancellation.cancelled() => return,
            result = media.write_all(&write.bytes) => result,
        };
        if result.is_err() {
            send_fatal(&supervisor_sender, "failed to write voice sidecar media");
            return;
        }
        if let Err(error) = shared.finish_media_write(write.operation_id) {
            let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
            return;
        }
    }
}

async fn run_stdout_reader(
    mut stdout: ChildStdout,
    max_payload_bytes: usize,
    session_id: SessionId,
    shared: Arc<SessionShared>,
    input_publisher: InputPublisher,
    cancellation: CancellationToken,
    supervisor_sender: mpsc::UnboundedSender<SupervisorEvent>,
) {
    let mut ready = false;
    loop {
        let control = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            frame = read_control_frame(&mut stdout, max_payload_bytes) => match frame {
                Ok(Some(control)) => control,
                Ok(None) => {
                    let _ = supervisor_sender.send(SupervisorEvent::Eof);
                    return;
                }
                Err(error) => {
                    let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                    return;
                }
            }
        };

        if control_session_id(&control) != session_id {
            send_fatal(
                &supervisor_sender,
                "voice sidecar session identity mismatch",
            );
            return;
        }
        if let SidecarControl::Failure { stage, code, .. } = control {
            let error = sidecar_failure(stage, code);
            input_publisher.publish_reliable(Err(error.clone()));
            let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
            return;
        }
        if !ready {
            if matches!(control, SidecarControl::Ready { .. }) {
                ready = true;
                let _ = supervisor_sender.send(SupervisorEvent::Ready);
                continue;
            }
            send_fatal(
                &supervisor_sender,
                "voice sidecar event arrived before readiness",
            );
            return;
        }

        let input_event = match control {
            SidecarControl::VoiceActivity { activity, .. } => {
                Some((VoiceInputEvent::Activity(activity), true))
            }
            SidecarControl::TranscriptHypothesis { hypothesis, .. } => Some((
                VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis.clone())),
                hypothesis.is_engine_final(),
            )),
            SidecarControl::PlaybackAccepted {
                turn_id,
                generation_id,
                utterance_id,
                sequence,
                ..
            } => {
                let identity = MediaIdentity {
                    turn_id,
                    generation_id,
                    utterance_id,
                    sequence,
                };
                if let Err(error) = resolve_media_ack(&shared, identity, &cancellation).await {
                    let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                    return;
                }
                None
            }
            SidecarControl::PlaybackRendered {
                turn_id,
                generation_id,
                utterance_id,
                sequence,
                ..
            } => {
                let identity = MediaIdentity {
                    turn_id,
                    generation_id,
                    utterance_id,
                    sequence,
                };
                match shared.resolve_render(identity) {
                    Ok(true) => Some((
                        VoiceInputEvent::Playback(PlaybackReceipt::new(
                            generation_id,
                            PlaybackState::Rendered,
                        )),
                        false,
                    )),
                    Ok(false) => None,
                    Err(error) => {
                        let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                        return;
                    }
                }
            }
            SidecarControl::PlaybackFlushed {
                generation_id,
                operation_id,
                ..
            } => {
                if let Err(error) = shared.resolve_flush(generation_id, operation_id) {
                    let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                    return;
                }
                None
            }
            SidecarControl::ShutdownComplete { .. } => {
                let _ = supervisor_sender.send(SupervisorEvent::ShutdownComplete);
                continue;
            }
            SidecarControl::Ready { .. }
            | SidecarControl::StartSession { .. }
            | SidecarControl::StartCapture { .. }
            | SidecarControl::FlushGeneration { .. }
            | SidecarControl::Shutdown { .. } => {
                send_fatal(
                    &supervisor_sender,
                    "voice sidecar lifecycle event was out of order",
                );
                return;
            }
            SidecarControl::Failure { .. } => unreachable!("failure handled before readiness"),
        };

        if let Some((event, reliable)) = input_event {
            let published = if reliable {
                input_publisher.publish_reliable(Ok(event))
            } else {
                input_publisher.publish_nonterminal(Ok(event));
                true
            };
            if !published {
                return;
            }
        }
    }
}

async fn resolve_media_ack(
    shared: &SessionShared,
    identity: MediaIdentity,
    cancellation: &CancellationToken,
) -> Result<(), AdapterError> {
    loop {
        match shared.resolve_media(identity)? {
            ResolveMedia::Complete => return Ok(()),
            ResolveMedia::WaitForWrite(written) => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Ok(()),
                    () = written.notified() => {}
                }
            }
        }
    }
}

async fn read_control_frame<R>(
    reader: &mut R,
    max_payload_bytes: usize,
) -> Result<Option<SidecarControl>, AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 8];
    let header_bytes = read_until_eof(reader, &mut header)
        .await
        .map_err(|_| AdapterError::new("failed to read voice sidecar event"))?;
    if header_bytes == 0 {
        return Ok(None);
    }
    if header_bytes < header.len() {
        let _ = decode_frame_at_eof(&header[..header_bytes]);
        return Err(AdapterError::new("voice sidecar event frame was truncated"));
    }
    let declared = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let declared = usize::try_from(declared)
        .map_err(|_| AdapterError::new("voice sidecar event length overflowed"))?;
    if declared > max_payload_bytes || declared > MAX_CONTROL_PAYLOAD_BYTES {
        return Err(AdapterError::new(
            "voice sidecar event exceeded the configured limit",
        ));
    }
    let capacity = header
        .len()
        .checked_add(declared)
        .ok_or_else(|| AdapterError::new("voice sidecar event length overflowed"))?;
    let mut frame = Vec::with_capacity(capacity);
    frame.extend_from_slice(&header);
    frame.resize(capacity, 0);
    let payload_bytes = read_until_eof(reader, &mut frame[header.len()..])
        .await
        .map_err(|_| AdapterError::new("voice sidecar event frame was truncated"))?;
    if payload_bytes < declared {
        frame.truncate(header.len() + payload_bytes);
        let _ = decode_frame_at_eof(&frame);
        return Err(AdapterError::new("voice sidecar event frame was truncated"));
    }
    let decoded = decode_frame(&frame)
        .map_err(|_| AdapterError::new("voice sidecar event frame was malformed"))?;
    debug_assert_eq!(decoded.version(), PROTOCOL_VERSION);
    if decoded.as_audio().is_some() {
        return Err(AdapterError::new(
            "voice sidecar emitted media on standard output",
        ));
    }
    decoded
        .as_control()
        .cloned()
        .ok_or_else(|| AdapterError::new("voice sidecar event frame was malformed"))
        .map(Some)
}

async fn read_until_eof<R>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

async fn run_stderr_reader(
    stderr: ChildStderr,
    limit: usize,
    supervisor_sender: mpsc::UnboundedSender<SupervisorEvent>,
) -> std::io::Result<Vec<u8>> {
    let result = read_bounded_prefix(stderr, limit).await;
    if result.is_err() {
        send_fatal(&supervisor_sender, "failed to read voice sidecar stderr");
    }
    result
}

async fn read_bounded_prefix<R>(mut reader: R, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

enum SupervisorEvent {
    Ready,
    ShutdownComplete,
    Fatal(AdapterError),
    Eof,
}

fn send_fatal(sender: &mpsc::UnboundedSender<SupervisorEvent>, message: &'static str) {
    let _ = sender.send(SupervisorEvent::Fatal(AdapterError::new(message)));
}

fn control_session_id(control: &SidecarControl) -> SessionId {
    match control {
        SidecarControl::StartSession { session_id, .. }
        | SidecarControl::StartCapture { session_id }
        | SidecarControl::FlushGeneration { session_id, .. }
        | SidecarControl::Shutdown { session_id }
        | SidecarControl::Ready { session_id }
        | SidecarControl::VoiceActivity { session_id, .. }
        | SidecarControl::TranscriptHypothesis { session_id, .. }
        | SidecarControl::PlaybackAccepted { session_id, .. }
        | SidecarControl::PlaybackRendered { session_id, .. }
        | SidecarControl::PlaybackFlushed { session_id, .. }
        | SidecarControl::Failure { session_id, .. }
        | SidecarControl::ShutdownComplete { session_id } => *session_id,
    }
}

fn sidecar_failure(stage: RuntimeStage, code: SidecarFailureCode) -> AdapterError {
    let message = match code {
        SidecarFailureCode::PermissionDenied => "voice sidecar permission denied",
        SidecarFailureCode::InvalidState => "voice sidecar reported invalid state",
        SidecarFailureCode::MalformedFrame => "voice sidecar reported a malformed frame",
        SidecarFailureCode::AudioDeviceUnavailable => "voice sidecar audio device unavailable",
        SidecarFailureCode::RecognitionFailed => "voice sidecar recognition failed",
        SidecarFailureCode::PlaybackFailed => "voice sidecar playback failed",
        SidecarFailureCode::Internal => "voice sidecar internal failure",
    };
    let expected_stage = match code {
        SidecarFailureCode::PermissionDenied | SidecarFailureCode::AudioDeviceUnavailable => {
            matches!(
                stage,
                RuntimeStage::AudioCapture | RuntimeStage::AudioOutput
            )
        }
        SidecarFailureCode::RecognitionFailed => stage == RuntimeStage::SpeechRecognizer,
        SidecarFailureCode::PlaybackFailed => {
            matches!(
                stage,
                RuntimeStage::AudioOutput | RuntimeStage::ContinuousAudioOutput
            )
        }
        SidecarFailureCode::InvalidState
        | SidecarFailureCode::MalformedFrame
        | SidecarFailureCode::Internal => stage == RuntimeStage::VoiceSidecar,
    };
    if expected_stage {
        AdapterError::new(message)
    } else {
        AdapterError::new("voice sidecar failure stage mismatch")
    }
}

struct MediaWrite {
    operation_id: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MediaIdentity {
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    sequence: u64,
}

impl From<&AudioFrame> for MediaIdentity {
    fn from(frame: &AudioFrame) -> Self {
        Self {
            turn_id: frame.turn_id(),
            generation_id: frame.generation_id(),
            utterance_id: frame.utterance_id(),
            sequence: frame.sequence(),
        }
    }
}

struct SessionShared {
    session_id: SessionId,
    state: Mutex<SessionState>,
}

struct SessionState {
    next_operation_id: u64,
    latest_generation: Option<GenerationId>,
    flushed_through: Option<GenerationId>,
    media_operations: VecDeque<MediaOperation>,
    cancelled_media: VecDeque<CancelledMedia>,
    flush_operations: VecDeque<FlushOperation>,
    cancelled_flushes: VecDeque<CancelledFlush>,
    failure: Option<AdapterError>,
}

struct MediaOperation {
    operation_id: u64,
    identity: MediaIdentity,
    write_state: MediaWriteState,
    write_finished: Arc<Notify>,
    accepted: bool,
    completion: Option<oneshot::Sender<Result<PlaybackReceipt, AdapterError>>>,
    reservation: Option<MediaReservation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaWriteState {
    Queued,
    Writing,
    Written,
}

struct CancelledMedia {
    operation_id: u64,
    identity: MediaIdentity,
    write_state: MediaWriteState,
    write_finished: Arc<Notify>,
    accepted: bool,
    reservation: Option<MediaReservation>,
}

struct FlushOperation {
    operation_id: u64,
    generation_id: GenerationId,
    completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
}

struct CancelledFlush {
    operation_id: u64,
    generation_id: GenerationId,
}

enum ResolveMedia {
    Complete,
    WaitForWrite(Arc<Notify>),
}

struct MediaOperationGuard {
    shared: Arc<SessionShared>,
    operation_id: u64,
    armed: bool,
}

impl MediaOperationGuard {
    fn new(shared: Arc<SessionShared>, operation_id: u64) -> Self {
        Self {
            shared,
            operation_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MediaOperationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.cancel_media(self.operation_id);
        }
    }
}

struct FlushOperationGuard {
    shared: Arc<SessionShared>,
    operation_id: u64,
    armed: bool,
}

impl FlushOperationGuard {
    fn new(shared: Arc<SessionShared>, operation_id: u64) -> Self {
        Self {
            shared,
            operation_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FlushOperationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.cancel_flush(self.operation_id);
        }
    }
}

impl SessionShared {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            state: Mutex::new(SessionState {
                next_operation_id: 1,
                latest_generation: None,
                flushed_through: None,
                media_operations: VecDeque::new(),
                cancelled_media: VecDeque::new(),
                flush_operations: VecDeque::new(),
                cancelled_flushes: VecDeque::new(),
                failure: None,
            }),
        }
    }

    fn validate_enqueue(&self, generation_id: GenerationId) -> Result<(), AdapterError> {
        let state = self.state.lock().expect("sidecar state lock poisoned");
        validate_generation(&state, generation_id, false)
    }

    fn register_media(
        &self,
        identity: MediaIdentity,
        completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
        reservation: MediaReservation,
    ) -> Result<u64, AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        validate_generation(&state, identity.generation_id, false)?;
        if state
            .media_operations
            .iter()
            .any(|operation| operation.identity == identity)
            || state
                .cancelled_media
                .iter()
                .any(|operation| operation.identity == identity)
        {
            return Err(AdapterError::new(
                "voice sidecar media frame identity duplicated",
            ));
        }
        if state
            .media_operations
            .len()
            .saturating_add(state.cancelled_media.len())
            >= MEDIA_QUEUE_CAPACITY
        {
            return Err(AdapterError::new(
                "voice sidecar outstanding media limit reached",
            ));
        }
        if state
            .latest_generation
            .is_none_or(|latest| identity.generation_id > latest)
        {
            state.latest_generation = Some(identity.generation_id);
        }
        let operation_id = state.next_operation_id;
        state.next_operation_id = state
            .next_operation_id
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("voice sidecar request identity overflowed"))?;
        state.media_operations.push_back(MediaOperation {
            operation_id,
            identity,
            write_state: MediaWriteState::Queued,
            write_finished: Arc::new(Notify::new()),
            accepted: false,
            completion: Some(completion),
            reservation: Some(reservation),
        });
        Ok(operation_id)
    }

    fn cancel_media(&self, operation_id: u64) {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(index) = state
            .media_operations
            .iter()
            .position(|operation| operation.operation_id == operation_id)
        {
            let mut operation = state
                .media_operations
                .remove(index)
                .expect("media operation index");
            operation.write_finished.notify_one();
            if let Some(completion) = operation.completion.take() {
                let _ = completion.send(Err(AdapterError::new("voice sidecar media discarded")));
            }
            if operation.write_state != MediaWriteState::Queued {
                state.cancelled_media.push_back(CancelledMedia {
                    operation_id,
                    identity: operation.identity,
                    write_state: operation.write_state,
                    write_finished: operation.write_finished,
                    accepted: operation.accepted,
                    reservation: operation.reservation,
                });
            }
        }
    }

    fn begin_media_write(&self, operation_id: u64) -> bool {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if state.failure.is_some() {
            return false;
        }
        let Some(operation) = state
            .media_operations
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
        else {
            return false;
        };
        if operation.write_state != MediaWriteState::Queued {
            return false;
        }
        operation.write_state = MediaWriteState::Writing;
        true
    }

    fn finish_media_write(&self, operation_id: u64) -> Result<(), AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(operation) = state
            .media_operations
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
        {
            if operation.write_state != MediaWriteState::Writing {
                return Err(AdapterError::new(
                    "voice sidecar media write state mismatch",
                ));
            }
            operation.write_state = MediaWriteState::Written;
            operation.write_finished.notify_one();
            return Ok(());
        }
        if let Some(operation) = state
            .cancelled_media
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
        {
            if operation.write_state != MediaWriteState::Writing {
                return Err(AdapterError::new(
                    "voice sidecar media write state mismatch",
                ));
            }
            operation.write_state = MediaWriteState::Written;
            operation.write_finished.notify_one();
        }
        Ok(())
    }

    fn resolve_media(&self, identity: MediaIdentity) -> Result<ResolveMedia, AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if state
            .flushed_through
            .is_some_and(|flushed| identity.generation_id <= flushed)
        {
            return Err(AdapterError::new("voice sidecar generation is stale"));
        }
        validate_generation(&state, identity.generation_id, true)?;
        if let Some(operation) = state
            .media_operations
            .iter_mut()
            .find(|operation| operation.identity == identity)
        {
            return match operation.write_state {
                MediaWriteState::Queued => Err(AdapterError::new(
                    "voice sidecar media acknowledgement preceded its write",
                )),
                MediaWriteState::Writing => Ok(ResolveMedia::WaitForWrite(Arc::clone(
                    &operation.write_finished,
                ))),
                MediaWriteState::Written if operation.accepted => Err(AdapterError::new(
                    "voice sidecar media acknowledgement duplicated",
                )),
                MediaWriteState::Written => {
                    operation.accepted = true;
                    let generation_id = operation.identity.generation_id;
                    if let Some(completion) = operation.completion.take() {
                        let _ = completion.send(Ok(PlaybackReceipt::new(
                            generation_id,
                            PlaybackState::Accepted,
                        )));
                    }
                    operation.reservation.take();
                    Ok(ResolveMedia::Complete)
                }
            };
        }
        if let Some(cancelled) = state
            .cancelled_media
            .iter_mut()
            .find(|cancelled| cancelled.identity == identity)
        {
            return match cancelled.write_state {
                MediaWriteState::Queued => Err(AdapterError::new(
                    "voice sidecar media acknowledgement preceded its write",
                )),
                MediaWriteState::Writing => Ok(ResolveMedia::WaitForWrite(Arc::clone(
                    &cancelled.write_finished,
                ))),
                MediaWriteState::Written if cancelled.accepted => Err(AdapterError::new(
                    "voice sidecar media acknowledgement duplicated",
                )),
                MediaWriteState::Written => {
                    cancelled.accepted = true;
                    cancelled.reservation.take();
                    Ok(ResolveMedia::Complete)
                }
            };
        }
        Err(AdapterError::new(
            "voice sidecar media acknowledgement mismatch",
        ))
    }

    fn resolve_render(&self, identity: MediaIdentity) -> Result<bool, AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if state
            .flushed_through
            .is_some_and(|flushed| identity.generation_id <= flushed)
        {
            return Err(AdapterError::new("voice sidecar generation is stale"));
        }
        validate_generation(&state, identity.generation_id, true)?;
        if let Some(index) = state
            .media_operations
            .iter()
            .position(|operation| operation.identity == identity)
        {
            if !state.media_operations[index].accepted {
                return Err(AdapterError::new(
                    "voice sidecar rendered media before acceptance",
                ));
            }
            state
                .media_operations
                .remove(index)
                .expect("media operation index");
            return Ok(true);
        }
        if let Some(index) = state
            .cancelled_media
            .iter()
            .position(|cancelled| cancelled.identity == identity)
        {
            if !state.cancelled_media[index].accepted {
                return Err(AdapterError::new(
                    "voice sidecar rendered media before acceptance",
                ));
            }
            state
                .cancelled_media
                .remove(index)
                .expect("cancelled media index");
            return Ok(false);
        }
        Err(AdapterError::new(
            "voice sidecar rendered media identity mismatch",
        ))
    }

    fn register_flush(
        &self,
        generation_id: GenerationId,
    ) -> Result<
        (
            u64,
            oneshot::Receiver<Result<PlaybackReceipt, AdapterError>>,
        ),
        AdapterError,
    > {
        let (completion, receiver) = oneshot::channel();
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        validate_flush_request(&state, generation_id)?;
        if state
            .latest_generation
            .is_none_or(|latest| generation_id > latest)
        {
            state.latest_generation = Some(generation_id);
        }
        if state
            .flushed_through
            .is_none_or(|flushed| generation_id > flushed)
        {
            state.flushed_through = Some(generation_id);
        }
        let operation_id = state.next_operation_id;
        state.next_operation_id = state
            .next_operation_id
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("voice sidecar request identity overflowed"))?;
        let mut retained = VecDeque::new();
        while let Some(operation) = state.media_operations.pop_front() {
            if operation.identity.generation_id <= generation_id {
                operation.write_finished.notify_one();
                if let Some(completion) = operation.completion {
                    let _ =
                        completion.send(Err(AdapterError::new("voice sidecar generation flushed")));
                }
            } else {
                retained.push_back(operation);
            }
        }
        state.media_operations = retained;
        let mut retained_cancelled = VecDeque::new();
        while let Some(cancelled) = state.cancelled_media.pop_front() {
            if cancelled.identity.generation_id <= generation_id {
                cancelled.write_finished.notify_one();
            } else {
                retained_cancelled.push_back(cancelled);
            }
        }
        state.cancelled_media = retained_cancelled;
        state
            .cancelled_flushes
            .retain(|cancelled| cancelled.generation_id > generation_id);
        state.flush_operations.push_back(FlushOperation {
            operation_id,
            generation_id,
            completion,
        });
        Ok((operation_id, receiver))
    }

    fn cancel_flush(&self, operation_id: u64) {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(index) = state
            .flush_operations
            .iter()
            .position(|operation| operation.operation_id == operation_id)
        {
            let operation = state
                .flush_operations
                .remove(index)
                .expect("flush operation index");
            if state.cancelled_flushes.len() >= CONTROL_QUEUE_CAPACITY {
                state.cancelled_flushes.pop_front();
            }
            state.cancelled_flushes.push_back(CancelledFlush {
                operation_id,
                generation_id: operation.generation_id,
            });
            let _ = operation
                .completion
                .send(Err(AdapterError::new("voice sidecar flush discarded")));
        }
    }

    fn resolve_flush(
        &self,
        generation_id: GenerationId,
        operation_id: u64,
    ) -> Result<(), AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(index) = state
            .flush_operations
            .iter()
            .position(|operation| operation.operation_id == operation_id)
        {
            if state.flush_operations[index].generation_id != generation_id {
                return Err(AdapterError::new(
                    "voice sidecar flush acknowledgement identity mismatch",
                ));
            }
            let operation = state
                .flush_operations
                .remove(index)
                .expect("flush operation index");
            let _ = operation.completion.send(Ok(PlaybackReceipt::new(
                generation_id,
                PlaybackState::Flushed,
            )));
            return Ok(());
        }
        if let Some(index) = state
            .cancelled_flushes
            .iter()
            .position(|operation| operation.operation_id == operation_id)
        {
            if state.cancelled_flushes[index].generation_id != generation_id {
                return Err(AdapterError::new(
                    "voice sidecar flush acknowledgement identity mismatch",
                ));
            }
            state
                .cancelled_flushes
                .remove(index)
                .expect("cancelled flush index");
            return Ok(());
        }
        if state
            .flushed_through
            .is_some_and(|flushed| generation_id <= flushed)
        {
            return Err(AdapterError::new("voice sidecar generation is stale"));
        }
        Err(AdapterError::new(
            "voice sidecar flush acknowledgement mismatch",
        ))
    }

    fn active_generation(&self) -> Option<GenerationId> {
        self.state
            .lock()
            .expect("sidecar state lock poisoned")
            .latest_generation
    }

    fn fail(&self, error: AdapterError) {
        let (media, flushes) = {
            let mut state = self.state.lock().expect("sidecar state lock poisoned");
            if state.failure.is_none() {
                state.failure = Some(error.clone());
            }
            for operation in &state.media_operations {
                operation.write_finished.notify_one();
            }
            for operation in &state.cancelled_media {
                operation.write_finished.notify_one();
            }
            state.cancelled_media.clear();
            state.cancelled_flushes.clear();
            (
                state.media_operations.drain(..).collect::<Vec<_>>(),
                state.flush_operations.drain(..).collect::<Vec<_>>(),
            )
        };
        for operation in media {
            if let Some(completion) = operation.completion {
                let _ = completion.send(Err(error.clone()));
            }
        }
        for operation in flushes {
            let _ = operation.completion.send(Err(error.clone()));
        }
    }
}

fn validate_flush_request(
    state: &SessionState,
    generation_id: GenerationId,
) -> Result<(), AdapterError> {
    if let Some(error) = &state.failure {
        return Err(error.clone());
    }
    if state
        .latest_generation
        .is_some_and(|latest| generation_id < latest)
    {
        return Err(AdapterError::new("voice sidecar generation is stale"));
    }
    Ok(())
}

fn validate_generation(
    state: &SessionState,
    generation_id: GenerationId,
    allow_flushed: bool,
) -> Result<(), AdapterError> {
    if let Some(error) = &state.failure {
        return Err(error.clone());
    }
    if state
        .latest_generation
        .is_some_and(|latest| generation_id < latest)
    {
        return Err(AdapterError::new("voice sidecar generation is stale"));
    }
    if !allow_flushed
        && state
            .flushed_through
            .is_some_and(|flushed| generation_id <= flushed)
    {
        return Err(AdapterError::new("voice sidecar generation is stale"));
    }
    if allow_flushed
        && state
            .latest_generation
            .is_some_and(|latest| generation_id > latest)
    {
        return Err(AdapterError::new(
            "voice sidecar generation identity mismatch",
        ));
    }
    Ok(())
}

struct MediaBudget {
    used_nanos: Mutex<u128>,
    released: Notify,
}

impl MediaBudget {
    fn new() -> Self {
        Self {
            used_nanos: Mutex::new(0),
            released: Notify::new(),
        }
    }

    async fn reserve(
        self: &Arc<Self>,
        duration_nanos: u128,
        frame_permit: OwnedSemaphorePermit,
        cancellation: &CancellationToken,
        session_cancellation: &CancellationToken,
    ) -> Result<MediaReservation, AdapterError> {
        if duration_nanos > MAX_QUEUED_MEDIA_NANOS {
            return Err(AdapterError::new(
                "voice sidecar frame exceeded two-second media limit",
            ));
        }
        loop {
            let released = self.released.notified();
            {
                let mut used = self.used_nanos.lock().expect("media budget lock poisoned");
                if used
                    .checked_add(duration_nanos)
                    .is_some_and(|total| total <= MAX_QUEUED_MEDIA_NANOS)
                {
                    *used += duration_nanos;
                    return Ok(MediaReservation {
                        budget: Arc::clone(self),
                        duration_nanos,
                        _frame_permit: frame_permit,
                    });
                }
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(AdapterError::new("voice sidecar media enqueue cancelled"));
                }
                _ = session_cancellation.cancelled() => {
                    return Err(AdapterError::new("voice sidecar session cancelled"));
                }
                () = released => {}
            }
        }
    }
}

struct MediaReservation {
    budget: Arc<MediaBudget>,
    duration_nanos: u128,
    _frame_permit: OwnedSemaphorePermit,
}

impl Drop for MediaReservation {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used_nanos
            .lock()
            .expect("media budget lock poisoned");
        *used = used.saturating_sub(self.duration_nanos);
        drop(used);
        self.budget.released.notify_waiters();
    }
}

fn frame_duration_nanos(frame: &AudioFrame) -> Result<u128, AdapterError> {
    let alignment = frame.format().frame_alignment_bytes()?;
    let sample_frames = frame.bytes().len() / alignment;
    let numerator = (sample_frames as u128)
        .checked_mul(1_000_000_000)
        .ok_or_else(|| AdapterError::new("voice sidecar media duration overflowed"))?;
    let denominator = u128::from(frame.format().sample_rate_hz());
    let rounded = numerator
        .checked_add(denominator.saturating_sub(1))
        .ok_or_else(|| AdapterError::new("voice sidecar media duration overflowed"))?;
    Ok(rounded / denominator)
}

fn require_absolute_path(path: &Path, field: &str) -> Result<(), AdapterError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(configuration_error(format!("{field} must be absolute")))
    }
}

fn require_executable_file(path: &Path) -> Result<(), AdapterError> {
    let metadata = std::fs::metadata(path)
        .map_err(|_| configuration_error("sidecar executable does not exist"))?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(configuration_error(
            "sidecar executable must be a regular file",
        ))
    }
}

fn configuration_error(message: impl AsRef<str>) -> AdapterError {
    AdapterError::new(format!(
        "invalid macOS voice sidecar configuration: {}",
        message.as_ref()
    ))
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[tokio::test]
    async fn writing_ack_waiters_are_released_by_cancel_flush_and_shutdown_cleanup() {
        let cancelled = Arc::new(SessionShared::new(SessionId::new(1)));
        let cancelled_identity = test_identity(1);
        let cancelled_operation = register_writing_media(&cancelled, cancelled_identity).await;
        let cancelled_wait = wait_for_write(cancelled.resolve_media(cancelled_identity).unwrap());
        cancelled.cancel_media(cancelled_operation);
        notified(cancelled_wait).await;
        let cancelled_finish_wait =
            wait_for_write(cancelled.resolve_media(cancelled_identity).unwrap());
        cancelled.finish_media_write(cancelled_operation).unwrap();
        notified(cancelled_finish_wait).await;
        assert!(matches!(
            cancelled.resolve_media(cancelled_identity).unwrap(),
            ResolveMedia::Complete
        ));

        let flushed = Arc::new(SessionShared::new(SessionId::new(2)));
        let flushed_identity = test_identity(2);
        let _flushed_operation = register_writing_media(&flushed, flushed_identity).await;
        let flushed_wait = wait_for_write(flushed.resolve_media(flushed_identity).unwrap());
        let _flush = flushed
            .register_flush(flushed_identity.generation_id)
            .unwrap();
        notified(flushed_wait).await;
        assert!(flushed.resolve_media(flushed_identity).is_err());

        let shutdown = Arc::new(SessionShared::new(SessionId::new(3)));
        let shutdown_identity = test_identity(3);
        let _shutdown_operation = register_writing_media(&shutdown, shutdown_identity).await;
        let shutdown_wait = wait_for_write(shutdown.resolve_media(shutdown_identity).unwrap());
        shutdown.fail(AdapterError::new("test shutdown"));
        notified(shutdown_wait).await;
        assert!(shutdown.resolve_media(shutdown_identity).is_err());
    }

    #[test]
    fn cancelled_flush_bookkeeping_never_exceeds_control_capacity() {
        let shared = SessionShared::new(SessionId::new(4));
        for _ in 0..(CONTROL_QUEUE_CAPACITY * 4) {
            let (operation_id, _completion) = shared.register_flush(GenerationId::new(1)).unwrap();
            shared.cancel_flush(operation_id);
        }
        let state = shared.state.lock().expect("sidecar state lock poisoned");
        assert!(state.cancelled_flushes.len() <= CONTROL_QUEUE_CAPACITY);
    }

    async fn register_writing_media(shared: &SessionShared, identity: MediaIdentity) -> u64 {
        let budget = Arc::new(MediaBudget::new());
        let capacity = Arc::new(Semaphore::new(1));
        let frame_permit = capacity.acquire_owned().await.unwrap();
        let cancellation = CancellationToken::new();
        let reservation = budget
            .reserve(1, frame_permit, &cancellation, &cancellation)
            .await
            .unwrap();
        let (completion, _receiver) = oneshot::channel();
        let operation_id = shared
            .register_media(identity, completion, reservation)
            .unwrap();
        assert!(shared.begin_media_write(operation_id));
        operation_id
    }

    fn test_identity(value: u64) -> MediaIdentity {
        MediaIdentity {
            turn_id: TurnId::new(value),
            generation_id: GenerationId::new(value),
            utterance_id: UtteranceId::new(value),
            sequence: value,
        }
    }

    fn wait_for_write(resolution: ResolveMedia) -> Arc<Notify> {
        match resolution {
            ResolveMedia::WaitForWrite(written) => written,
            ResolveMedia::Complete => panic!("media acknowledgement resolved before write"),
        }
    }

    async fn notified(written: Arc<Notify>) {
        tokio::time::timeout(Duration::from_millis(100), written.notified())
            .await
            .expect("media write waiter was orphaned");
    }
}
