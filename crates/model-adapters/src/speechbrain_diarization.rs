//! Local batch diarization behind the provider-neutral port: a sidecar process, spoken to over
//! JSON Lines on stdio.
//!
//! The adapter is handed one bounded window of decrypted meeting audio at a time and forwards
//! each window to a sidecar it spawns for that one reconciliation. Two properties follow from
//! that shape rather than from anyone's good behaviour:
//!
//! - **No plaintext file exists at any point.** The sidecar is given bytes on a pipe, never a
//!   path, so there is nothing to remove on success, on failure, or on cancellation. Nothing
//!   here opens a file; the only path the adapter knows is the sidecar executable's.
//! - **Nothing leaves the machine.** The sidecar has no network use of its own: the model is
//!   loaded from a local cache directory the operator populated. A deployment can therefore
//!   diarize a meeting without a provider ever hearing it.
//!
//! The local backend **diarizes and does not transcribe.** SpeechBrain's speaker embeddings
//! answer who sounds like whom, not what was said, so every segment carries an empty text. That
//! is deliberately visible in the result rather than hidden behind invented words.
//!
//! # The clock
//!
//! The sidecar clusters the whole meeting at once, so it answers on the **media timeline**
//! directly — the same origin and units as the finalized recording summary's `duration_ms` and
//! its gaps — rather than per window. A segment that ends past the audio the adapter actually
//! sent is refused: an out-of-range timing is a plausible wrong answer rather than a visible
//! failure.
//!
//! # Diagnostics
//!
//! The sidecar's stderr is discarded and its prose never travels. A failure it reports names one
//! code from a closed list, which this adapter checks against its own copy of that list; a code
//! nobody declared is refused rather than relayed, so the vocabulary cannot be widened from the
//! far side of the pipe.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::diarization::{
    DiarizationResult, DiarizationWarning, DiarizedSegment, MeetingAudioWindow,
    MeetingAudioWindows, SpeakerClusters, TranscriptionDiarizer, MAX_DIARIZED_SEGMENTS,
    MAX_SPEAKER_CLUSTERS,
};
use crate::{AdapterError, AdapterFuture, PcmFormat, PcmSampleFormat};

/// The protocol both sides implement. A sidecar that announces anything else is refused.
const PROTOCOL_VERSION: u16 = 1;
/// The longest answer line accepted, matching the sidecar's own cap and the client frame limit:
/// an answer past this cannot become a result the runtime would state.
const MAX_ANSWER_LINE_BYTES: usize = 512 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_CACHE_DIRECTORY_BYTES: usize = 1024;
const MIN_SPEAKERS: u16 = 2;
/// Loading a speaker-embedding model off a cold disk is seconds, not milliseconds.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Clustering a long meeting is minutes of CPU, and it all happens after the last window.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a sidecar has to exit once it has answered or been killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How far past the audio it was given a sidecar timing may fall before the segment is refused.
/// Frames round at their edges; a second is tolerance for that, not permission to invent a
/// timeline.
const TIMING_TOLERANCE_MS: u64 = 1_000;

/// Every failure the sidecar may name. Kept in step with the sidecar's own `FAILURE_CODES`;
/// a code outside it is not a diagnostic this runtime will repeat.
const FAILURE_CODES: [&str; 6] = [
    "request_invalid",
    "audio_unsupported",
    "pipeline_unavailable",
    "model_unavailable",
    "result_too_large",
    "internal_error",
];

/// Configuration for the local diarization sidecar.
///
/// There is no credential and no endpoint: this backend talks to a process, not a provider.
/// `model_cache_dir` is where the operator put the checkpoint; the sidecar loads from there with
/// the Hugging Face offline switch set, so a missing model fails closed instead of reaching the
/// network mid-meeting.
#[derive(Clone, Debug)]
pub struct SpeechbrainDiarizationConfig {
    executable: PathBuf,
    model: String,
    model_cache_dir: PathBuf,
    max_speakers: u16,
    startup_timeout: Duration,
    request_timeout: Duration,
}

impl SpeechbrainDiarizationConfig {
    pub fn new(
        executable: impl AsRef<Path>,
        model: impl Into<String>,
        model_cache_dir: impl AsRef<Path>,
        max_speakers: u16,
    ) -> Result<Self, AdapterError> {
        let executable = executable.as_ref();
        require_absolute(executable, "sidecar executable")?;
        require_executable_file(executable)?;
        let model = model.into();
        validate_configured_text(&model, MAX_MODEL_BYTES, "model identifiers")?;
        let model_cache_dir = model_cache_dir.as_ref();
        require_absolute(model_cache_dir, "model cache directory")?;
        if model_cache_dir.as_os_str().len() > MAX_CACHE_DIRECTORY_BYTES
            || model_cache_dir.to_str().is_none()
            || model_cache_dir
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(invalid_config(
                "model cache directory must be bounded UTF-8 and must not traverse parents",
            ));
        }
        if !(MIN_SPEAKERS..=speaker_ceiling()).contains(&max_speakers) {
            return Err(invalid_config(
                "maximum speaker count is outside the supported range",
            ));
        }

        Ok(Self {
            executable: executable.to_path_buf(),
            model,
            model_cache_dir: model_cache_dir.to_path_buf(),
            max_speakers,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    pub fn with_startup_timeout(mut self, startup_timeout: Duration) -> Result<Self, AdapterError> {
        if startup_timeout.is_zero() {
            return Err(invalid_config("startup timeout must be non-zero"));
        }
        self.startup_timeout = startup_timeout;
        Ok(self)
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Result<Self, AdapterError> {
        if request_timeout.is_zero() {
            return Err(invalid_config("request timeout must be non-zero"));
        }
        self.request_timeout = request_timeout;
        Ok(self)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn model_cache_dir(&self) -> &Path {
        &self.model_cache_dir
    }

    pub const fn max_speakers(&self) -> u16 {
        self.max_speakers
    }
}

/// Batch transcription and diarization against a local sidecar process.
#[derive(Debug)]
pub struct SpeechbrainTranscriptionDiarizer {
    config: SpeechbrainDiarizationConfig,
}

impl SpeechbrainTranscriptionDiarizer {
    pub const fn new(config: SpeechbrainDiarizationConfig) -> Self {
        Self { config }
    }
}

impl TranscriptionDiarizer for SpeechbrainTranscriptionDiarizer {
    fn diarize<'a>(
        &'a self,
        windows: MeetingAudioWindows,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        Box::pin(async move { self.run(windows, &cancellation).await })
    }
}

impl SpeechbrainTranscriptionDiarizer {
    async fn run(
        &self,
        mut windows: MeetingAudioWindows,
        cancellation: &CancellationToken,
    ) -> Result<DiarizationResult, AdapterError> {
        // The first window decides the audio format the sidecar is started with, so nothing is
        // spawned for a recording that holds no audio at all.
        let Some(first) = next_window(&mut windows, cancellation).await? else {
            return DiarizationResult::new(Vec::new(), vec![DiarizationWarning::NoSpeechDetected]);
        };
        let mut child = self.spawn()?;
        let outcome = self
            .session(&mut child, first, &mut windows, cancellation)
            .await;
        // Reaped on every path. A run that failed has already said why, so the reaping's own
        // outcome is only allowed to fail a run that otherwise succeeded.
        let reaped = reap(&mut child, outcome.is_ok()).await;
        outcome.and_then(|result| reaped.map(|()| result))
    }

    fn spawn(&self) -> Result<Child, AdapterError> {
        Command::new(self.config.executable())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The sidecar's prose is not a diagnostic this runtime relays: a traceback can quote
            // a model path or a filename, and everything the runtime needs travels as a code.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| AdapterError::new("failed to spawn local diarization sidecar"))
    }

    async fn session(
        &self,
        child: &mut Child,
        first: MeetingAudioWindow,
        windows: &mut MeetingAudioWindows,
        cancellation: &CancellationToken,
    ) -> Result<DiarizationResult, AdapterError> {
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(AdapterError::new(
                "failed to open local diarization sidecar pipes",
            ));
        };
        let (sender, mut answers) = mpsc::channel(2);
        let reader = tokio::spawn(read_answers(stdout, sender));

        let result = self
            .exchange(
                child,
                &mut stdin,
                &mut answers,
                first,
                windows,
                cancellation,
            )
            .await;
        reader.abort();
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &self,
        child: &mut Child,
        stdin: &mut ChildStdin,
        answers: &mut mpsc::Receiver<Result<SidecarAnswer, AdapterError>>,
        first: MeetingAudioWindow,
        windows: &mut MeetingAudioWindows,
        cancellation: &CancellationToken,
    ) -> Result<DiarizationResult, AdapterError> {
        self.send(stdin, &self.start_line(first.format())?, cancellation)
            .await?;
        match resolve(
            child,
            answers.recv(),
            self.config.startup_timeout,
            cancellation,
        )
        .await?
        {
            SidecarAnswer::Ready { protocol_version } if protocol_version == PROTOCOL_VERSION => {}
            _ => return Err(unsupported_answer()),
        }

        let mut state = Reconciliation::default();
        let mut window = Some(first);
        while let Some(current) = window {
            state.observe(&current);
            let line = audio_line(&current)?;
            drop(current);
            // The write races the sidecar's own voice: a sidecar that gave up on window three of
            // two hundred stops reading, and pushing the rest of the meeting into a pipe nobody
            // drains would stall until the timeout instead of reporting what it said.
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancelled()),
                early = answers.recv() => {
                    resolve_received(child, early).await?;
                    return Err(unsupported_answer());
                }
                sent = timeout(self.config.request_timeout, stdin.write_all(&line)) => {
                    sent.map_err(|_| stalled())?.map_err(|_| transport_failed())?;
                }
            }
            window = next_window(windows, cancellation).await?;
        }

        self.send(stdin, FINISH_LINE.as_bytes(), cancellation)
            .await?;
        // Closing the write half is what a cancelled reconciliation would leave behind, so the
        // sidecar treats it as the end of the recording either way.
        let _ = stdin.shutdown().await;

        match resolve(
            child,
            answers.recv(),
            self.config.request_timeout,
            cancellation,
        )
        .await?
        {
            SidecarAnswer::Answer { segments, warnings } => state.accept(segments, &warnings),
            SidecarAnswer::Ready { .. } => Err(unsupported_answer()),
        }
    }

    async fn send(
        &self,
        stdin: &mut ChildStdin,
        line: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<(), AdapterError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            written = timeout(self.config.request_timeout, stdin.write_all(line)) => {
                written.map_err(|_| stalled())?.map_err(|_| transport_failed())
            }
        }
    }

    fn start_line(&self, format: PcmFormat) -> Result<Vec<u8>, AdapterError> {
        let Some(model_cache_dir) = self.config.model_cache_dir.to_str() else {
            return Err(invalid_config("model cache directory must be UTF-8"));
        };
        encode_line(&StartRequest {
            r#type: "start",
            protocol_version: PROTOCOL_VERSION,
            model: &self.config.model,
            model_cache_dir,
            max_speakers: self.config.max_speakers,
            encoding: sidecar_encoding(format)?,
            sample_rate_hz: format.sample_rate_hz(),
            channels: format.channels(),
        })
    }
}

const FINISH_LINE: &str = "{\"type\":\"finish\"}\n";

fn audio_line(window: &MeetingAudioWindow) -> Result<Vec<u8>, AdapterError> {
    encode_line(&AudioRequest {
        r#type: "audio",
        started_at_ms: window.started_at_ms(),
        audio: crate::base64::encode(window.bytes()),
    })
}

fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, AdapterError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|_| AdapterError::new("local diarization request could not be encoded"))?;
    line.push(b'\n');
    Ok(line)
}

async fn next_window(
    windows: &mut MeetingAudioWindows,
    cancellation: &CancellationToken,
) -> Result<Option<MeetingAudioWindow>, AdapterError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled()),
        window = windows.recv() => window.transpose(),
    }
}

/// Waits for the sidecar's next word, bounded by `limit` and by cancellation.
async fn resolve(
    child: &mut Child,
    receiving: impl std::future::Future<Output = Option<Result<SidecarAnswer, AdapterError>>>,
    limit: Duration,
    cancellation: &CancellationToken,
) -> Result<SidecarAnswer, AdapterError> {
    let received = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancelled()),
        received = timeout(limit, receiving) => received.map_err(|_| stalled())?,
    };
    resolve_received(child, received).await
}

/// Turns what the reader task produced into an answer or a reason there is none. A closed
/// channel means the sidecar stopped talking, which is only ever explained by its exit.
async fn resolve_received(
    child: &mut Child,
    received: Option<Result<SidecarAnswer, AdapterError>>,
) -> Result<SidecarAnswer, AdapterError> {
    match received {
        Some(answer) => answer,
        None => {
            let _ = timeout(EXIT_TIMEOUT, child.wait()).await;
            Err(AdapterError::new(
                "local diarization sidecar exited without answering",
            ))
        }
    }
}

/// Waits for the sidecar to go, killing it when it will not. `graceful` states whether a clean
/// exit is still expected: a run that already failed is not owed one.
async fn reap(child: &mut Child, graceful: bool) -> Result<(), AdapterError> {
    if graceful {
        if let Ok(Ok(status)) = timeout(EXIT_TIMEOUT, child.wait()).await {
            return if status.success() {
                Ok(())
            } else {
                Err(AdapterError::new(
                    "local diarization sidecar exited with a failure status",
                ))
            };
        }
    }
    let _ = child.start_kill();
    let _ = timeout(EXIT_TIMEOUT, child.wait()).await;
    if graceful {
        Err(AdapterError::new(
            "local diarization sidecar did not exit after answering",
        ))
    } else {
        Ok(())
    }
}

/// Reads the sidecar's lines until it answers, fails, or stops talking.
async fn read_answers(
    stdout: ChildStdout,
    sender: mpsc::Sender<Result<SidecarAnswer, AdapterError>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let answer = match read_line(&mut reader).await {
            Ok(Some(line)) => parse_answer(&line),
            Ok(None) => return,
            Err(error) => Err(error),
        };
        let terminal = !matches!(answer, Ok(SidecarAnswer::Ready { .. }));
        if sender.send(answer).await.is_err() || terminal {
            return;
        }
    }
}

/// One line, refused the moment it passes the cap rather than once it has all arrived.
async fn read_line(reader: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>, AdapterError> {
    let mut line = Vec::new();
    loop {
        let (chunk, complete) = {
            let available = reader.fill_buf().await.map_err(|_| transport_failed())?;
            if available.is_empty() {
                return Ok((!line.is_empty()).then_some(line));
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => (available[..=index].to_vec(), true),
                None => (available.to_vec(), false),
            }
        };
        reader.consume(chunk.len());
        if chunk.len() > MAX_ANSWER_LINE_BYTES.saturating_sub(line.len()) {
            return Err(response_limit());
        }
        line.extend_from_slice(&chunk);
        if complete {
            return Ok(Some(line));
        }
    }
}

fn parse_answer(line: &[u8]) -> Result<SidecarAnswer, AdapterError> {
    match serde_json::from_slice::<SidecarLine>(line) {
        Ok(SidecarLine::Ready { protocol_version }) => {
            Ok(SidecarAnswer::Ready { protocol_version })
        }
        Ok(SidecarLine::Answer { segments, warnings }) => {
            Ok(SidecarAnswer::Answer { segments, warnings })
        }
        Ok(SidecarLine::Failure { code }) if FAILURE_CODES.contains(&code.as_str()) => Err(
            AdapterError::new(format!("local diarization sidecar failed: {code}")),
        ),
        Ok(SidecarLine::Failure { .. }) | Err(_) => Err(unsupported_answer()),
    }
}

/// What the adapter has accepted so far, and the mapping that keeps the sidecar's own labels
/// out of it.
#[derive(Default)]
struct Reconciliation {
    media_end_ms: u64,
    clusters: SpeakerClusters,
}

impl Reconciliation {
    fn observe(&mut self, window: &MeetingAudioWindow) {
        self.media_end_ms = self
            .media_end_ms
            .max(window.started_at_ms().saturating_add(window.audio_ms()));
    }

    fn accept(
        mut self,
        answered: Vec<SidecarSegment>,
        warnings: &[String],
    ) -> Result<DiarizationResult, AdapterError> {
        if answered.len() > MAX_DIARIZED_SEGMENTS {
            return Err(AdapterError::new("diarization exceeded its segment limit"));
        }
        let limit = self.media_end_ms.saturating_add(TIMING_TOLERANCE_MS);
        let mut segments = Vec::with_capacity(answered.len());
        let mut previous_end_ms = 0;
        for segment in answered {
            // The sidecar clusters frames in order, so its turns never overlap and never run
            // backwards. A reply that does is not this backend's answer.
            if segment.end_ms < segment.start_ms
                || segment.end_ms > limit
                || segment.start_ms < previous_end_ms
            {
                return Err(unusable_timings());
            }
            previous_end_ms = segment.end_ms;
            let cluster = segment
                .speaker
                .as_deref()
                .map(|label| self.clusters.label_for(label))
                .transpose()?;
            segments.push(DiarizedSegment::new(
                format!("seg-{}", segments.len() + 1),
                segment.start_ms,
                segment.end_ms,
                // This backend hears voices, not words. The text is empty by construction.
                "",
                cluster,
            )?);
        }

        let mut collected = Vec::new();
        if segments.is_empty() {
            collected.push(DiarizationWarning::NoSpeechDetected);
        }
        if !warnings.is_empty() {
            collected.push(DiarizationWarning::ProviderReportedWarnings);
        }
        DiarizationResult::new(segments, collected)
    }
}

#[derive(Serialize)]
struct StartRequest<'a> {
    r#type: &'a str,
    protocol_version: u16,
    model: &'a str,
    model_cache_dir: &'a str,
    max_speakers: u16,
    encoding: &'a str,
    sample_rate_hz: u32,
    channels: u16,
}

#[derive(Serialize)]
struct AudioRequest<'a> {
    r#type: &'a str,
    started_at_ms: u64,
    audio: String,
}

/// The sidecar's vocabulary, as it arrives.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum SidecarLine {
    Ready {
        protocol_version: u16,
    },
    #[serde(rename = "result")]
    Answer {
        segments: Vec<SidecarSegment>,
        #[serde(default)]
        warnings: Vec<String>,
    },
    Failure {
        code: String,
    },
}

/// The same vocabulary once a failure has become an error, which is where it belongs.
enum SidecarAnswer {
    Ready {
        protocol_version: u16,
    },
    Answer {
        segments: Vec<SidecarSegment>,
        warnings: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarSegment {
    start_ms: u64,
    end_ms: u64,
    #[serde(default)]
    speaker: Option<String>,
}

/// The sidecar's name for the window's sample format. Exhaustive on purpose: a new sample
/// format must be given a spelling both sides know rather than silently reaching the pipe.
fn sidecar_encoding(format: PcmFormat) -> Result<&'static str, AdapterError> {
    match format.sample_format() {
        PcmSampleFormat::Signed16LittleEndian => Ok("pcm_s16le"),
        PcmSampleFormat::Float32LittleEndian => Err(AdapterError::new(
            "local diarization supports 16-bit audio only",
        )),
    }
}

const fn speaker_ceiling() -> u16 {
    // The port refuses a result carrying more clusters than this, so configuring more would be
    // configuring a failure.
    MAX_SPEAKER_CLUSTERS as u16
}

fn require_absolute(path: &Path, field: &str) -> Result<(), AdapterError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(invalid_config(&format!("{field} must be absolute")))
    }
}

fn require_executable_file(path: &Path) -> Result<(), AdapterError> {
    let metadata =
        std::fs::metadata(path).map_err(|_| invalid_config("sidecar executable does not exist"))?;
    if !metadata.is_file() {
        return Err(invalid_config("sidecar executable must be a regular file"));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(invalid_config("sidecar executable is not executable"));
    }
    Ok(())
}

fn validate_configured_text(
    value: &str,
    maximum_bytes: usize,
    subject: &str,
) -> Result<(), AdapterError> {
    if value.trim().is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control)
    {
        return Err(invalid_config(&format!("{subject} must be bounded text")));
    }
    Ok(())
}

fn invalid_config(detail: &str) -> AdapterError {
    AdapterError::new(format!("invalid local diarization configuration: {detail}"))
}

fn transport_failed() -> AdapterError {
    AdapterError::new("local diarization sidecar transport failed")
}

fn stalled() -> AdapterError {
    AdapterError::new("local diarization sidecar stalled")
}

fn response_limit() -> AdapterError {
    AdapterError::new("local diarization answer exceeded the configured limit")
}

fn unsupported_answer() -> AdapterError {
    AdapterError::new("local diarization sidecar returned an unsupported answer")
}

fn unusable_timings() -> AdapterError {
    AdapterError::new("local diarization sidecar returned a segment with unusable timings")
}

fn cancelled() -> AdapterError {
    AdapterError::new("local diarization was cancelled")
}

#[cfg(test)]
mod tests {
    use super::{parse_answer, SpeechbrainDiarizationConfig};

    fn executable() -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("diarization-sidecar");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        (directory, path)
    }

    #[test]
    fn a_configuration_refuses_what_it_cannot_run() {
        let (directory, path) = executable();
        let cache = directory.path();

        assert!(SpeechbrainDiarizationConfig::new(&path, "model", cache, 12).is_ok());
        assert!(SpeechbrainDiarizationConfig::new("relative/sidecar", "m", cache, 12).is_err());
        assert!(SpeechbrainDiarizationConfig::new(directory.path(), "m", cache, 12).is_err());
        assert!(SpeechbrainDiarizationConfig::new(&path, "", cache, 12).is_err());
        assert!(SpeechbrainDiarizationConfig::new(&path, "m", "relative", 12).is_err());
        assert!(SpeechbrainDiarizationConfig::new(&path, "m", "/models/../etc", 12).is_err());
        assert!(SpeechbrainDiarizationConfig::new(&path, "m", cache, 1).is_err());
        assert!(SpeechbrainDiarizationConfig::new(&path, "m", cache, 65).is_err());
    }

    #[test]
    fn a_failure_code_nobody_declared_is_not_repeated() {
        let known = parse_answer(br#"{"type":"failure","code":"model_unavailable"}"#)
            .err()
            .expect("a failure is not an answer");
        assert_eq!(
            known.message(),
            "local diarization sidecar failed: model_unavailable"
        );

        let invented = parse_answer(br#"{"type":"failure","code":"chris was not recognised"}"#)
            .err()
            .expect("a failure is not an answer");
        assert_eq!(
            invented.message(),
            "local diarization sidecar returned an unsupported answer"
        );
        assert!(parse_answer(b"not json").is_err());
        assert!(parse_answer(br#"{"type":"result","segments":[],"extra":1}"#).is_err());
    }
}
