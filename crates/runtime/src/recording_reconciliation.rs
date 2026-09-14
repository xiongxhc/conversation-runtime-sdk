//! Post-meeting reconciliation: an encrypted recording, reached by handle, becomes timed text
//! and anonymous speaker clusters.
//!
//! This module is the runtime's adapter boundary for a finished recording. It is the only
//! place a recording is decrypted for a provider, and it decrypts in **bounded in-memory
//! windows** that are streamed straight to the diarizer. No plaintext file is created on any
//! path — not on success, not on failure, not on cancellation — because none is ever created
//! at all. The key never leaves this host and reaches nothing but the reader.
//!
//! # The clock
//!
//! A window's `started_at_ms` is on the **media timeline**: the same origin and units as a
//! finalized recording summary's `duration_ms` and its gap timestamps. The timeline advances by
//! the audio a recording holds plus the wall-clock length of the gaps *between* that audio, and
//! this reader accumulates it exactly as the sink did — one frame at a time, from the frame's
//! own sample count, and one gap at a time, from the two wall-clock timestamps the file carries
//! around it. A gap that opened before any audio arrived contributes nothing, because the
//! recording had not started.
//!
//! Every `at_ms` inside the encrypted file is on the recorder's wall clock instead. The two
//! clocks agree only for a recording that began immediately and ran without a gap, which is
//! why this reader correlates by chunk order and by the gap records rather than by treating one
//! clock's value as meaningful on the other.

use std::path::Path;
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, DiarizationResult, DiarizationWarning, MeetingAudioWindow, PcmFormat,
    TranscriptionDiarizer,
};
use conversation_protocol::{
    FinalizedRecordingCapability, RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session_recording::{
    open_session_recording, pcm_duration_ms, preflight_finalized_recording,
    resolve_recording_handle, RecordedChunk, RecordedChunks, RecordingSettings,
};

/// The most plaintext meeting audio one window holds.
///
/// A mebibyte is about 33 seconds of 16 kHz mono signed 16-bit audio and roughly 1.4 MB once
/// base64 encoded into a request body — the same order as the streaming recognizer's upload
/// chunk. A two-hour meeting is around 230 MB, which this turns into some 225 windows; the
/// resident cost is one window being filled plus the one in flight, whatever the meeting's
/// length. Sizing a buffer from a recording's *duration* would be wrong: the duration is the
/// media timeline and includes silence no audio was recorded for.
pub const MAX_RECONCILIATION_WINDOW_BYTES: usize = 1024 * 1024;

/// One window in flight. The reader is a blocking task, so a deeper queue would only buy
/// resident plaintext.
const WINDOW_CHANNEL_CAPACITY: usize = 1;

/// Authorizes a finalized recording independently of the current gateway identity. Complete
/// bounded authentication and decoding precede the first provider call. The second pass uses
/// the same private encrypted snapshot, never a reopened caller-named path.
pub async fn reconcile_finalized_recording(
    settings: &RecordingSettings,
    capability: &FinalizedRecordingCapability,
    diarizer: Arc<dyn TranscriptionDiarizer>,
    cancellation: CancellationToken,
) -> Result<DiarizationResult, RuntimeError> {
    let settings = settings.clone();
    let capability = capability.clone();
    let preflight_cancellation = cancellation.clone();
    let chunks = tokio::task::spawn_blocking(move || {
        preflight_finalized_recording(&settings, &capability, &preflight_cancellation)
    })
    .await
    .map_err(|_| adapter_error("recording preflight did not finish"))??;
    if cancellation.is_cancelled() {
        return Err(invalid_state_error("recording recovery was cancelled"));
    }
    let (sender, receiver) = mpsc::channel(WINDOW_CHANNEL_CAPACITY);
    let reader_cancellation = cancellation.clone();
    let reader = tokio::task::spawn_blocking(move || {
        read_chunk_windows(chunks, &sender, &reader_cancellation)
    });
    let result = diarizer.diarize(receiver, cancellation.clone()).await;
    let read = reader
        .await
        .map_err(|_| adapter_error("recording reader did not finish"))?;
    read?;
    if cancellation.is_cancelled() {
        return Err(invalid_state_error("recording recovery was cancelled"));
    }
    result.map_err(|error| adapter_runtime_error(&error))
}

/// Reconciles the recording named by `handle` into timed text and anonymous clusters.
///
/// Access is by handle alone: [`resolve_recording_handle`] refuses a file that does not carry
/// the binding identifier the handle spells, which is what isolates one recording from another
/// when — as the gateway does — every meeting runs under the same session identifier. The
/// session identifier is then checked against the header as provenance; it is deliberately not
/// what provides the isolation.
pub async fn reconcile_recording(
    settings: &RecordingSettings,
    session_id: SessionId,
    handle: &str,
    diarizer: Arc<dyn TranscriptionDiarizer>,
    cancellation: CancellationToken,
) -> Result<DiarizationResult, RuntimeError> {
    let path = resolve_recording_handle(settings.directory(), handle)?;
    let key_env = settings.key_env().to_owned();
    let (sender, receiver) = mpsc::channel(WINDOW_CHANNEL_CAPACITY);
    let reader_cancellation = cancellation.clone();

    // Decryption is CPU work over a file, so it runs off the reactor. It pushes windows down a
    // bounded channel, which is what makes the whole walk cost one window of memory.
    let reader = tokio::task::spawn_blocking(move || {
        read_windows(&path, &key_env, session_id, &sender, &reader_cancellation)
    });

    let diarized = diarizer.diarize(receiver, cancellation).await;
    let read = reader
        .await
        .map_err(|_| adapter_error("recording reader did not finish"))?;

    // A reader failure outranks the diarizer's answer: a diarizer handed no windows reports no
    // speech, which would otherwise hide the real cause.
    let truncated = read?;
    let result = diarized.map_err(|error| adapter_runtime_error(&error))?;
    Ok(if truncated {
        result.with_warning(DiarizationWarning::RecordingTruncated)
    } else {
        result
    })
}

/// Walks the recording and sends bounded windows. Returns whether the recording's tail was
/// torn, which is what a crash mid-write leaves behind.
fn read_windows(
    path: &Path,
    key_env: &str,
    session_id: SessionId,
    sender: &mpsc::Sender<Result<MeetingAudioWindow, AdapterError>>,
    cancellation: &CancellationToken,
) -> Result<bool, RuntimeError> {
    let chunks = open_session_recording(path, key_env)?;
    if chunks.session_id() != session_id {
        return Err(invalid_state_error(
            "recording was not made by that session",
        ));
    }

    read_chunk_windows(chunks, sender, cancellation)
}

fn read_chunk_windows(
    mut chunks: RecordedChunks,
    sender: &mpsc::Sender<Result<MeetingAudioWindow, AdapterError>>,
    cancellation: &CancellationToken,
) -> Result<bool, RuntimeError> {
    let mut windows = Windows::default();
    loop {
        if cancellation.is_cancelled() {
            return Ok(chunks.truncated());
        }
        let Some(chunk) = chunks.next_chunk()? else {
            break;
        };
        let delivered = match chunk {
            RecordedChunk::Frame {
                sample_rate_hz,
                channels,
                sample_format,
                bytes,
                ..
            } => {
                let format = PcmFormat::new(sample_rate_hz, channels, sample_format)
                    .map_err(|_| adapter_error("recording frame states an unusable format"))?;
                windows.accept(format, bytes, sender)?
            }
            RecordedChunk::GapStarted { at_ms, .. } => {
                // A window never spans a gap: the audio either side of it is not contiguous,
                // and a provider handed one buffer would time the second half as if it were.
                let delivered = windows.flush(sender)?;
                windows.gap_opened_at_ms = Some(at_ms);
                delivered
            }
            RecordedChunk::GapEnded { at_ms } => {
                windows.close_gap(at_ms);
                true
            }
            RecordedChunk::Ended { .. } => windows.flush(sender)?,
        };
        if !delivered {
            // The diarizer stopped listening — cancelled, or finished with an error of its
            // own. Its result is what the caller will see.
            return Ok(chunks.truncated());
        }
    }
    windows.flush(sender)?;
    Ok(chunks.truncated())
}

/// The window under construction, and the media timeline it sits on.
#[derive(Default)]
struct Windows {
    format: Option<PcmFormat>,
    /// The audio recorded so far plus the gaps between it, in milliseconds.
    media_ms: u64,
    /// Where the window being filled starts on that timeline.
    started_at_ms: u64,
    bytes: Vec<u8>,
    /// The recorder's wall-clock timestamp of an open gap, which is the only clock the file
    /// states a gap's length on.
    gap_opened_at_ms: Option<u64>,
}

impl Windows {
    /// Adds one frame's audio, flushing first when it would overflow the window. Reports
    /// whether the diarizer is still listening.
    fn accept(
        &mut self,
        format: PcmFormat,
        bytes: Vec<u8>,
        sender: &mpsc::Sender<Result<MeetingAudioWindow, AdapterError>>,
    ) -> Result<bool, RuntimeError> {
        match self.format {
            Some(existing) if existing != format => {
                return Err(adapter_error(
                    "recording capture format changed mid-session",
                ));
            }
            Some(_) => {}
            None => self.format = Some(format),
        }
        if bytes.len() > MAX_RECONCILIATION_WINDOW_BYTES {
            return Err(adapter_error("recording frame exceeded the window size"));
        }
        if self.bytes.len() + bytes.len() > MAX_RECONCILIATION_WINDOW_BYTES
            && !self.flush(sender)?
        {
            return Ok(false);
        }
        if self.bytes.is_empty() {
            self.started_at_ms = self.media_ms;
        }
        self.media_ms = self
            .media_ms
            .saturating_add(pcm_duration_ms(format, bytes.len()));
        self.bytes.extend_from_slice(&bytes);
        Ok(true)
    }

    /// Closes an open gap, charging the timeline the wall-clock silence it covered.
    ///
    /// The length charged is the difference of the two timestamps the file itself carries, and
    /// the sink charges its own timeline with exactly the same difference. That is what makes
    /// the reconstructed timeline equal the summary's rather than approximate it: this reader
    /// consults no clock of its own, so the agreement is by construction and not by luck.
    ///
    /// A gap that opened before any audio was recorded costs nothing: the recording had not
    /// started, so there is no timeline for it to occupy. This is the sink's rule, read back.
    fn close_gap(&mut self, ended_at_ms: u64) {
        let Some(opened_at_ms) = self.gap_opened_at_ms.take() else {
            return;
        };
        if self.format.is_some() {
            self.media_ms = self
                .media_ms
                .saturating_add(ended_at_ms.saturating_sub(opened_at_ms));
        }
    }

    /// Sends the window being filled, if it holds anything. Reports whether the diarizer is
    /// still listening.
    fn flush(
        &mut self,
        sender: &mpsc::Sender<Result<MeetingAudioWindow, AdapterError>>,
    ) -> Result<bool, RuntimeError> {
        if self.bytes.is_empty() {
            return Ok(true);
        }
        let format = self
            .format
            .ok_or_else(|| adapter_error("recording window has no format"))?;
        let window =
            MeetingAudioWindow::new(format, self.started_at_ms, std::mem::take(&mut self.bytes))
                .map_err(|error| adapter_runtime_error(&error))?;
        Ok(sender.blocking_send(Ok(window)).is_ok())
    }
}

fn adapter_runtime_error(error: &AdapterError) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Adapter,
        error.stage().unwrap_or(RuntimeStage::AudioCapture),
        error.message(),
    )
}

fn adapter_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::AudioCapture,
        message,
    )
}

fn invalid_state_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::AudioCapture,
        message,
    )
}
