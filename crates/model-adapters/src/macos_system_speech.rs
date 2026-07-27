use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioFormat, SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
};

const DEFAULT_MAX_TEXT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_STDERR_BYTES: usize = 8 * 1024;
const STDERR_CLEANUP_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacOsSystemSpeechConfig {
    executable: PathBuf,
    voice: Option<String>,
    rate: Option<u32>,
    max_text_bytes: usize,
    max_audio_bytes: usize,
    max_stderr_bytes: usize,
    temp_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MacOsSystemSpeechSynthesizer {
    config: MacOsSystemSpeechConfig,
}

impl MacOsSystemSpeechConfig {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self, AdapterError> {
        let executable = executable.as_ref();
        require_absolute_path(executable, "speech executable")?;

        Ok(Self {
            executable: executable.to_path_buf(),
            voice: None,
            rate: None,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            temp_directory: std::env::temp_dir(),
        })
    }

    pub fn system_default() -> Result<Self, AdapterError> {
        #[cfg(target_os = "macos")]
        {
            Self::new("/usr/bin/say")
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(AdapterError::new(
                "macOS system speech is unavailable on this platform",
            ))
        }
    }

    pub fn with_voice(mut self, voice: impl Into<String>) -> Result<Self, AdapterError> {
        let voice = voice.into();
        if voice.is_empty() || voice.chars().any(char::is_control) {
            return Err(configuration_error(
                "voice must be non-empty and contain no control characters",
            ));
        }
        self.voice = Some(voice);
        Ok(self)
    }

    pub fn with_rate(mut self, rate: u32) -> Result<Self, AdapterError> {
        if rate == 0 {
            return Err(configuration_error("rate must be non-zero"));
        }
        self.rate = Some(rate);
        Ok(self)
    }

    pub fn with_max_text_bytes(mut self, max_text_bytes: usize) -> Result<Self, AdapterError> {
        self.max_text_bytes = require_non_zero(max_text_bytes, "text byte limit")?;
        Ok(self)
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

    pub fn voice(&self) -> Option<&str> {
        self.voice.as_deref()
    }

    pub const fn rate(&self) -> Option<u32> {
        self.rate
    }

    pub const fn max_text_bytes(&self) -> usize {
        self.max_text_bytes
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

impl MacOsSystemSpeechSynthesizer {
    pub const fn new(config: MacOsSystemSpeechConfig) -> Self {
        Self { config }
    }
}

impl SpeechSynthesizer for MacOsSystemSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            validate_request(&request, self.config.max_text_bytes())?;
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("speech synthesis cancelled"));
            }

            let output = tempfile::Builder::new()
                .prefix("conversation-runtime-")
                .suffix(".aiff")
                .tempfile_in(self.config.temp_directory())
                .map_err(|_| AdapterError::new("failed to create speech synthesis output"))?;

            let mut command = Command::new(self.config.executable());
            command.arg("-o").arg(output.path());
            if let Some(voice) = self.config.voice() {
                command.arg("-v").arg(voice);
            }
            if let Some(rate) = self.config.rate() {
                command.arg("-r").arg(rate.to_string());
            }
            command.arg("--").arg(request.text());
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = command
                .spawn()
                .map_err(|_| AdapterError::new("failed to start speech synthesis"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| AdapterError::new("failed to capture speech synthesis error"))?;
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
                    return Err(AdapterError::new("speech synthesis cancelled"));
                }
                status = child.wait() => {
                    status.map_err(|_| AdapterError::new("failed to wait for speech synthesis"))?
                }
            };
            let stderr = stderr_task
                .await
                .map_err(|_| AdapterError::new("failed to read speech synthesis error"))?
                .map_err(|_| AdapterError::new("failed to read speech synthesis error"))?;

            if !status.success() {
                let detail = sanitize_stderr(&stderr);
                let message = if detail.is_empty() {
                    "speech synthesis process failed".to_owned()
                } else {
                    format!("speech synthesis process failed: {detail}")
                };
                return Err(AdapterError::new(message));
            }

            let bytes = read_audio(output.path(), self.config.max_audio_bytes()).await?;
            Ok(SynthesizedAudio::new(bytes, AudioFormat::Aiff))
        })
    }
}

fn validate_request(request: &SpeechRequest, max_text_bytes: usize) -> Result<(), AdapterError> {
    if request.text().is_empty() {
        return Err(AdapterError::new("speech synthesis text must not be empty"));
    }
    if request.text().len() > max_text_bytes {
        return Err(AdapterError::new(
            "speech synthesis text exceeded the configured limit",
        ));
    }
    Ok(())
}

async fn read_audio(path: &Path, max_audio_bytes: usize) -> Result<Vec<u8>, AdapterError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| AdapterError::new("failed to read speech synthesis output"))?;
    let mut bytes = Vec::new();
    let read_limit = u64::try_from(max_audio_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| AdapterError::new("failed to read speech synthesis output"))?;

    if bytes.is_empty() {
        return Err(AdapterError::new("speech synthesis output was empty"));
    }
    if bytes.len() > max_audio_bytes {
        return Err(AdapterError::new(
            "speech synthesis output exceeded the configured limit",
        ));
    }
    Ok(bytes)
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
        "invalid macOS system speech configuration: {}",
        message.as_ref()
    ))
}
