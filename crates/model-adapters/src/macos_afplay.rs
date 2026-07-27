use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
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

        Ok(Self {
            executable: executable.to_path_buf(),
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
            max_stderr_bytes: DEFAULT_MAX_ERROR_BYTES,
            temp_directory: std::env::temp_dir(),
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
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| AdapterError::new("failed to capture audio playback error"))?;
            let stderr_limit = self.config.max_stderr_bytes();
            let mut stderr_task =
                tokio::spawn(async move { read_bounded_prefix(stderr, stderr_limit).await });

            let status = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    if tokio::time::timeout(STDERR_CLEANUP_GRACE, &mut stderr_task)
                        .await
                        .is_err()
                    {
                        stderr_task.abort();
                        let _ = stderr_task.await;
                    }
                    return Err(AdapterError::new("audio playback cancelled"));
                }
                status = child.wait() => {
                    status.map_err(|_| AdapterError::new("failed to wait for audio playback"))?
                }
            };
            let stderr = stderr_task
                .await
                .map_err(|_| AdapterError::new("failed to read audio playback error"))?
                .map_err(|_| AdapterError::new("failed to read audio playback error"))?;

            if !status.success() {
                let detail = sanitize_stderr(&stderr);
                let message = if detail.is_empty() {
                    "audio playback process failed".to_owned()
                } else {
                    format!("audio playback process failed: {detail}")
                };
                return Err(AdapterError::new(message));
            }

            Ok(())
        })
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

fn sanitize_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
