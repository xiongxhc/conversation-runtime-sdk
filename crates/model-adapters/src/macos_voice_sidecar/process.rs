use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use command_fds::{CommandFdExt, FdMapping};
use conversation_protocol::{GenerationId, PlaybackState, RuntimeStage, SessionId};
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
    let (supervisor_sender, mut supervisor_receiver) = mpsc::unbounded_channel();
    let io_cancellation = CancellationToken::new();
    let media_cancellation = CancellationToken::new();

    let tasks = ProcessTasks {
        control: tokio::spawn(run_control_writer(
            stdin,
            control_receiver,
            io_cancellation.clone(),
            supervisor_sender.clone(),
        )),
        media: tokio::spawn(run_media_writer(
            media,
            media_receiver,
            Arc::clone(&shared),
            media_cancellation.clone(),
            io_cancellation.clone(),
            supervisor_sender.clone(),
        )),
        stdout: tokio::spawn(run_stdout_reader(
            stdout,
            config.max_payload_bytes(),
            session_id,
            Arc::clone(&shared),
            input_sender.clone(),
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
                    input_sender,
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
            let request_id = self.shared.register_media(
                frame.generation_id(),
                completion_sender,
                reservation,
            )?;
            let write = MediaWrite {
                request_id,
                generation_id: frame.generation_id(),
                bytes: encoded,
            };

            let sent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => false,
                _ = self.session_cancellation.cancelled() => false,
                result = self.media_sender.send(write) => result.is_ok(),
            };
            if !sent {
                self.shared.remove_media(request_id);
                return if cancellation.is_cancelled() {
                    Err(AdapterError::new("voice sidecar media enqueue cancelled"))
                } else if self.session_cancellation.is_cancelled() {
                    Err(AdapterError::new("voice sidecar session cancelled"))
                } else {
                    Err(AdapterError::new("voice sidecar media queue closed"))
                };
            }

            tokio::select! {
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
            }
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
            let completion_receiver = self.shared.register_flush(generation_id)?;
            let frame = SidecarFrame::control(SidecarControl::FlushGeneration {
                session_id,
                generation_id,
            });
            if self.control_sender.send(frame).await.is_err() {
                self.shared.remove_flush(generation_id);
                return Err(AdapterError::new("voice sidecar control queue closed"));
            }
            tokio::select! {
                biased;
                _ = self.session_cancellation.cancelled() => {
                    Err(AdapterError::new("voice sidecar session cancelled"))
                }
                result = completion_receiver => {
                    result.unwrap_or_else(|_| {
                        Err(AdapterError::new("voice sidecar flush acknowledgement closed"))
                    })
                }
            }
        })
    }
}

struct ProcessTasks {
    control: JoinHandle<()>,
    media: JoinHandle<()>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
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
    input_sender: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
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
            child_reaped = graceful_shutdown(
                &mut self.child,
                &mut self.events,
                &self.control_sender,
                &self.shared,
            )
            .await;
        } else {
            let _ = self.input_sender.try_send(Err(outcome.clone()));
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
        if let Some(generation_id) = shared.active_generation() {
            if let Ok(completion) = shared.register_flush(generation_id) {
                drop(completion);
                control_sender
                    .send(SidecarFrame::control(SidecarControl::FlushGeneration {
                        session_id: shared.session_id,
                        generation_id,
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
    tasks: ProcessTasks,
    shared: &Arc<SessionShared>,
    media_cancellation: &CancellationToken,
    io_cancellation: &CancellationToken,
    failure: AdapterError,
) -> Result<(), AdapterError> {
    shared.fail(failure);
    media_cancellation.cancel();
    io_cancellation.cancel();

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
    finish_task(tasks.media).await;
    finish_task(tasks.stdout).await;
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
        if !shared.should_write_media(write.generation_id) {
            shared.remove_media(write.request_id);
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
    }
}

async fn run_stdout_reader(
    mut stdout: ChildStdout,
    max_payload_bytes: usize,
    session_id: SessionId,
    shared: Arc<SessionShared>,
    input_sender: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
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
            let _ = input_sender.try_send(Err(error.clone()));
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
                Some(VoiceInputEvent::Activity(activity))
            }
            SidecarControl::TranscriptHypothesis { hypothesis, .. } => Some(
                VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)),
            ),
            SidecarControl::PlaybackAccepted { generation_id, .. } => {
                if let Err(error) = shared.resolve_media(generation_id) {
                    let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                    return;
                }
                None
            }
            SidecarControl::PlaybackRendered { generation_id, .. } => {
                if let Err(error) = shared.validate_render(generation_id) {
                    let _ = supervisor_sender.send(SupervisorEvent::Fatal(error));
                    return;
                }
                Some(VoiceInputEvent::Playback(PlaybackReceipt::new(
                    generation_id,
                    PlaybackState::Rendered,
                )))
            }
            SidecarControl::PlaybackFlushed { generation_id, .. } => {
                if let Err(error) = shared.resolve_flush(generation_id) {
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

        if let Some(event) = input_event {
            let sent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                result = input_sender.send(Ok(event)) => result,
            };
            if sent.is_err() {
                send_fatal(&supervisor_sender, "voice sidecar input consumer closed");
                return;
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
    request_id: u64,
    generation_id: GenerationId,
    bytes: Vec<u8>,
}

struct SessionShared {
    session_id: SessionId,
    state: Mutex<SessionState>,
}

struct SessionState {
    next_request_id: u64,
    latest_generation: Option<GenerationId>,
    flushed_through: Option<GenerationId>,
    pending_media: VecDeque<PendingMedia>,
    pending_flushes: VecDeque<PendingFlush>,
    failure: Option<AdapterError>,
}

struct PendingMedia {
    request_id: u64,
    generation_id: GenerationId,
    completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
    _reservation: MediaReservation,
}

struct PendingFlush {
    generation_id: GenerationId,
    completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
}

impl SessionShared {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            state: Mutex::new(SessionState {
                next_request_id: 1,
                latest_generation: None,
                flushed_through: None,
                pending_media: VecDeque::new(),
                pending_flushes: VecDeque::new(),
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
        generation_id: GenerationId,
        completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
        reservation: MediaReservation,
    ) -> Result<u64, AdapterError> {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        validate_generation(&state, generation_id, false)?;
        if state
            .latest_generation
            .is_none_or(|latest| generation_id > latest)
        {
            state.latest_generation = Some(generation_id);
        }
        let request_id = state.next_request_id;
        state.next_request_id = state
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("voice sidecar request identity overflowed"))?;
        state.pending_media.push_back(PendingMedia {
            request_id,
            generation_id,
            completion,
            _reservation: reservation,
        });
        Ok(request_id)
    }

    fn remove_media(&self, request_id: u64) {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(index) = state
            .pending_media
            .iter()
            .position(|pending| pending.request_id == request_id)
        {
            let pending = state
                .pending_media
                .remove(index)
                .expect("pending media index");
            let _ = pending
                .completion
                .send(Err(AdapterError::new("voice sidecar media discarded")));
        }
    }

    fn should_write_media(&self, generation_id: GenerationId) -> bool {
        let state = self.state.lock().expect("sidecar state lock poisoned");
        state.failure.is_none()
            && state
                .flushed_through
                .is_none_or(|flushed| generation_id > flushed)
    }

    fn resolve_media(&self, generation_id: GenerationId) -> Result<(), AdapterError> {
        let pending = {
            let mut state = self.state.lock().expect("sidecar state lock poisoned");
            if state
                .flushed_through
                .is_some_and(|flushed| generation_id <= flushed)
            {
                return Ok(());
            }
            validate_generation(&state, generation_id, true)?;
            let index = state
                .pending_media
                .iter()
                .position(|pending| pending.generation_id == generation_id)
                .ok_or_else(|| AdapterError::new("voice sidecar media acknowledgement mismatch"))?;
            state
                .pending_media
                .remove(index)
                .expect("pending media index")
        };
        let _ = pending.completion.send(Ok(PlaybackReceipt::new(
            generation_id,
            PlaybackState::Accepted,
        )));
        Ok(())
    }

    fn validate_render(&self, generation_id: GenerationId) -> Result<(), AdapterError> {
        let state = self.state.lock().expect("sidecar state lock poisoned");
        if state
            .flushed_through
            .is_some_and(|flushed| generation_id <= flushed)
        {
            return Ok(());
        }
        validate_generation(&state, generation_id, true)
    }

    fn register_flush(
        &self,
        generation_id: GenerationId,
    ) -> Result<oneshot::Receiver<Result<PlaybackReceipt, AdapterError>>, AdapterError> {
        let (completion, receiver) = oneshot::channel();
        let discarded = {
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
            let mut discarded = Vec::new();
            let mut retained = VecDeque::new();
            while let Some(pending) = state.pending_media.pop_front() {
                if pending.generation_id <= generation_id {
                    discarded.push(pending);
                } else {
                    retained.push_back(pending);
                }
            }
            state.pending_media = retained;
            state.pending_flushes.push_back(PendingFlush {
                generation_id,
                completion,
            });
            discarded
        };
        for pending in discarded {
            let _ = pending
                .completion
                .send(Err(AdapterError::new("voice sidecar generation flushed")));
        }
        Ok(receiver)
    }

    fn remove_flush(&self, generation_id: GenerationId) {
        let mut state = self.state.lock().expect("sidecar state lock poisoned");
        if let Some(index) = state
            .pending_flushes
            .iter()
            .position(|pending| pending.generation_id == generation_id)
        {
            let pending = state
                .pending_flushes
                .remove(index)
                .expect("pending flush index");
            let _ = pending
                .completion
                .send(Err(AdapterError::new("voice sidecar flush discarded")));
        }
    }

    fn resolve_flush(&self, generation_id: GenerationId) -> Result<(), AdapterError> {
        let pending = {
            let mut state = self.state.lock().expect("sidecar state lock poisoned");
            validate_generation(&state, generation_id, true)?;
            let index = state
                .pending_flushes
                .iter()
                .position(|pending| pending.generation_id == generation_id)
                .ok_or_else(|| AdapterError::new("voice sidecar flush acknowledgement mismatch"))?;
            state
                .pending_flushes
                .remove(index)
                .expect("pending flush index")
        };
        let _ = pending.completion.send(Ok(PlaybackReceipt::new(
            generation_id,
            PlaybackState::Flushed,
        )));
        Ok(())
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
            (
                state.pending_media.drain(..).collect::<Vec<_>>(),
                state.pending_flushes.drain(..).collect::<Vec<_>>(),
            )
        };
        for pending in media {
            let _ = pending.completion.send(Err(error.clone()));
        }
        for pending in flushes {
            let _ = pending.completion.send(Err(error.clone()));
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
