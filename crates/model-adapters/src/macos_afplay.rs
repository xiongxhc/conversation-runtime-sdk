use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, Command};
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, AudioFormat, AudioOutput, AudioOutputRequest};

const DEFAULT_MAX_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_ERROR_BYTES: usize = 4 * 1024;
const STDERR_CLEANUP_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacOsAfplayConfig {
    executable: PathBuf,
    max_audio_bytes: usize,
    max_stderr_bytes: usize,
    temp_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MacOsAfplayAudioOutput {
    config: MacOsAfplayConfig,
}

impl MacOsAfplayConfig {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self, AdapterError> {
        let executable = executable.as_ref();
        require_absolute_path(executable, "afplay executable")?;
        let temp_directory = std::env::temp_dir();
        require_absolute_path(&temp_directory, "temporary directory")?;

        Ok(Self {
            executable: executable.to_path_buf(),
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
            max_stderr_bytes: DEFAULT_MAX_ERROR_BYTES,
            temp_directory,
        })
    }

    pub fn system_default() -> Result<Self, AdapterError> {
        #[cfg(target_os = "macos")]
        {
            Self::new("/usr/bin/afplay")
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(AdapterError::new(
                "macOS afplay audio output is unavailable on this platform",
            ))
        }
    }

    pub fn with_max_audio_bytes(mut self, max_audio_bytes: usize) -> Result<Self, AdapterError> {
        self.max_audio_bytes = require_non_zero(max_audio_bytes, "audio byte limit")?;
        Ok(self)
    }

    pub fn with_max_stderr_bytes(mut self, max_stderr_bytes: usize) -> Result<Self, AdapterError> {
        self.max_stderr_bytes = require_non_zero(max_stderr_bytes, "stderr byte limit")?;
        Ok(self)
    }

    pub fn with_temp_directory(
        mut self,
        temp_directory: impl AsRef<Path>,
    ) -> Result<Self, AdapterError> {
        let temp_directory = temp_directory.as_ref();
        require_absolute_path(temp_directory, "temporary directory")?;
        self.temp_directory = temp_directory.to_path_buf();
        Ok(self)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub const fn max_audio_bytes(&self) -> usize {
        self.max_audio_bytes
    }

    pub const fn max_stderr_bytes(&self) -> usize {
        self.max_stderr_bytes
    }

    pub fn temp_directory(&self) -> &Path {
        &self.temp_directory
    }
}

impl MacOsAfplayAudioOutput {
    pub const fn new(config: MacOsAfplayConfig) -> Self {
        Self { config }
    }
}

impl AudioOutput for MacOsAfplayAudioOutput {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("audio playback cancelled"));
            }
            validate_request(&request, self.config.max_audio_bytes())?;

            let audio = request.audio();
            let mut input = tempfile::Builder::new()
                .prefix("conversation-runtime-")
                .suffix(audio_suffix(audio.format()))
                .tempfile_in(self.config.temp_directory())
                .map_err(|_| AdapterError::new("failed to create audio playback input"))?;
            input
                .write_all(audio.bytes())
                .map_err(|_| AdapterError::new("failed to write audio playback input"))?;

            let mut command = Command::new(self.config.executable());
            command
                .arg(input.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = command
                .spawn()
                .map_err(|_| AdapterError::new("failed to start audio playback"))?;
            play_spawned_process(&mut child, cancellation, self.config.max_stderr_bytes()).await
        })
    }
}

trait PlaybackProcess {
    type Stderr: AsyncRead + Unpin + Send + 'static;

    fn take_stderr(&mut self) -> Option<Self::Stderr>;
    fn start_kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = std::io::Result<bool>> + Send + '_>>;
}

impl PlaybackProcess for Child {
    type Stderr = ChildStderr;

    fn take_stderr(&mut self) -> Option<Self::Stderr> {
        self.stderr.take()
    }

    fn start_kill(&mut self) -> std::io::Result<()> {
        Child::start_kill(self)
    }

    fn wait(&mut self) -> Pin<Box<dyn Future<Output = std::io::Result<bool>> + Send + '_>> {
        Box::pin(async move { Child::wait(self).await.map(|status| status.success()) })
    }
}

enum PlaybackOutcome {
    Exited(bool),
    Failed(AdapterError),
}

impl PlaybackOutcome {
    const fn requires_termination(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

async fn play_spawned_process<P>(
    process: &mut P,
    cancellation: CancellationToken,
    stderr_limit: usize,
) -> Result<(), AdapterError>
where
    P: PlaybackProcess,
{
    let Some(stderr) = process.take_stderr() else {
        return finalize_playback(
            process,
            None,
            PlaybackOutcome::Failed(AdapterError::new("failed to capture audio playback error")),
        )
        .await;
    };
    let stderr_task = tokio::spawn(async move { read_bounded_prefix(stderr, stderr_limit).await });

    let outcome = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            PlaybackOutcome::Failed(AdapterError::new("audio playback cancelled"))
        }
        status = process.wait() => {
            match status {
                Ok(success) => PlaybackOutcome::Exited(success),
                Err(_) => PlaybackOutcome::Failed(AdapterError::new(
                    "failed to wait for audio playback",
                )),
            }
        }
    };

    finalize_playback(process, Some(stderr_task), outcome).await
}

async fn finalize_playback<P>(
    process: &mut P,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>,
    outcome: PlaybackOutcome,
) -> Result<(), AdapterError>
where
    P: PlaybackProcess,
{
    let child_cleanup = if outcome.requires_termination() {
        let _ = process.start_kill();
        process
            .wait()
            .await
            .map(|_| ())
            .map_err(|_| AdapterError::new("failed to wait for audio playback"))
    } else {
        Ok(())
    };
    let stderr_cleanup = match stderr_task {
        Some(stderr_task) => finish_stderr_task(stderr_task).await,
        None => Ok(()),
    };
    let cleanup_error = child_cleanup.err().or_else(|| stderr_cleanup.err());

    match outcome {
        PlaybackOutcome::Exited(true) => cleanup_error.map_or(Ok(()), Err),
        PlaybackOutcome::Exited(false) => Err(AdapterError::new("audio playback process failed")),
        PlaybackOutcome::Failed(error) => Err(error),
    }
}

fn validate_request(
    request: &AudioOutputRequest,
    max_audio_bytes: usize,
) -> Result<(), AdapterError> {
    let audio = request.audio();
    if audio.bytes().len() > max_audio_bytes {
        return Err(AdapterError::new(
            "audio output exceeded the configured limit",
        ));
    }
    audio.validate()
}

const fn audio_suffix(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Aiff => ".aiff",
        AudioFormat::Wav => ".wav",
    }
}

async fn read_bounded_prefix<R>(mut reader: R, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

async fn finish_stderr_task(
    mut stderr_task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<(), AdapterError> {
    match tokio::time::timeout(STDERR_CLEANUP_GRACE, &mut stderr_task).await {
        Ok(result) => result
            .map_err(|_| AdapterError::new("failed to read audio playback error"))?
            .map_err(|_| AdapterError::new("failed to read audio playback error"))
            .map(|_| ()),
        Err(_) => {
            stderr_task.abort();
            match stderr_task.await {
                Err(error) if error.is_cancelled() => Ok(()),
                Err(_) => Err(AdapterError::new("failed to read audio playback error")),
                Ok(result) => result
                    .map_err(|_| AdapterError::new("failed to read audio playback error"))
                    .map(|_| ()),
            }
        }
    }
}

fn require_absolute_path(path: &Path, field: &str) -> Result<(), AdapterError> {
    if !path.is_absolute() {
        return Err(configuration_error(format!("{field} must be absolute")));
    }
    Ok(())
}

fn require_non_zero(value: usize, field: &str) -> Result<usize, AdapterError> {
    if value == 0 {
        return Err(configuration_error(format!("{field} must be non-zero")));
    }
    Ok(value)
}

fn configuration_error(message: impl AsRef<str>) -> AdapterError {
    AdapterError::new(format!(
        "invalid macOS afplay configuration: {}",
        message.as_ref()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, ReadBuf};
    use tokio_util::sync::CancellationToken;

    use super::{play_spawned_process, PlaybackProcess};

    struct FakePlaybackProcess<R> {
        stderr: Option<R>,
        wait_results: VecDeque<std::io::Result<bool>>,
        kill_calls: usize,
        wait_calls: usize,
    }

    impl<R> FakePlaybackProcess<R> {
        fn new(
            stderr: Option<R>,
            wait_results: impl IntoIterator<Item = std::io::Result<bool>>,
        ) -> Self {
            Self {
                stderr,
                wait_results: wait_results.into_iter().collect(),
                kill_calls: 0,
                wait_calls: 0,
            }
        }
    }

    impl<R> PlaybackProcess for FakePlaybackProcess<R>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        type Stderr = R;

        fn take_stderr(&mut self) -> Option<Self::Stderr> {
            self.stderr.take()
        }

        fn start_kill(&mut self) -> std::io::Result<()> {
            self.kill_calls += 1;
            Ok(())
        }

        fn wait(&mut self) -> Pin<Box<dyn Future<Output = std::io::Result<bool>> + Send + '_>> {
            self.wait_calls += 1;
            let result = self
                .wait_results
                .pop_front()
                .expect("fake process wait result exhausted");
            Box::pin(async move { result })
        }
    }

    struct PendingReader {
        dropped: Arc<AtomicBool>,
    }

    impl AsyncRead for PendingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl Drop for PendingReader {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn missing_stderr_capture_kills_and_waits_before_returning_the_primary_error() {
        let mut process = FakePlaybackProcess::<tokio::io::Empty>::new(None, [Ok(true)]);

        let error = play_spawned_process(&mut process, CancellationToken::new(), 16)
            .await
            .unwrap_err();

        assert_eq!(error.message(), "failed to capture audio playback error");
        assert_eq!(process.kill_calls, 1);
        assert_eq!(process.wait_calls, 1);
    }

    #[tokio::test]
    async fn wait_failure_kills_waits_again_and_finishes_stderr_before_returning() {
        let mut process = FakePlaybackProcess::new(
            Some(tokio::io::empty()),
            [
                Err(std::io::Error::other("injected wait failure")),
                Ok(true),
            ],
        );

        let error = play_spawned_process(&mut process, CancellationToken::new(), 16)
            .await
            .unwrap_err();

        assert_eq!(error.message(), "failed to wait for audio playback");
        assert_eq!(process.kill_calls, 1);
        assert_eq!(process.wait_calls, 2);
    }

    #[tokio::test]
    async fn timed_out_stderr_reader_is_terminated_before_returning() {
        let stderr_dropped = Arc::new(AtomicBool::new(false));
        let mut process = FakePlaybackProcess::new(
            Some(PendingReader {
                dropped: Arc::clone(&stderr_dropped),
            }),
            [Ok(true)],
        );

        play_spawned_process(&mut process, CancellationToken::new(), 16)
            .await
            .unwrap();

        assert!(stderr_dropped.load(Ordering::Acquire));
        assert_eq!(process.kill_calls, 0);
        assert_eq!(process.wait_calls, 1);
    }
}
