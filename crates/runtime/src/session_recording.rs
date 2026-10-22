//! Encrypted session recording.
//!
//! The sink sits on the media path and appends every capture frame the bridge delivers as its
//! own AEAD chunk, so plaintext meeting audio is never written anywhere and a process that dies
//! mid-write still leaves a file that decrypts up to its last whole chunk.
//!
//! # On-disk format (`CRSREC02`, opt-in timed `CRSREC03`)
//!
//! ```text
//! header (48 bytes, plaintext, carries no secret)
//!   0..8    magic  b"CRSREC02" or explicitly timed b"CRSREC03"
//!   8       AEAD identifier (1 = AES-256-GCM)
//!   9..12   reserved, zero
//!   12..20  session identifier, u64 big endian (provenance only)
//!   20..28  nonce prefix, 8 random bytes drawn once per recording
//!   28..44  binding identifier, 16 random bytes drawn once per recording
//!   44..48  reserved, zero
//! ```
//!
//! The magic names the layout, not the product: `CRSREC01` was the 32-byte header without a
//! binding identifier and is refused by name rather than left to fail on a mis-seeked chunk
//! cursor. Reserved bytes must be zero, so a future field cannot be silently ignored.
//!
//! ```text
//! chunk (repeated)
//!   0..4    ciphertext length, u32 big endian
//!   4..     ciphertext = sealed record plus its 16-byte tag
//! ```
//!
//! The nonce for chunk `n` is `nonce_prefix || n as u32 big endian`, so a nonce is never reused
//! within a recording and two recordings under the same key start from different prefixes. The
//! additional authenticated data is `header || n`, which binds every chunk to its file and to
//! its position: a reordered, spliced or edited chunk fails to open. Truncation at a chunk
//! boundary is therefore lossless, and truncation inside a chunk loses only that chunk.
//!
//! The binding identifier is what the handle names: the handle is its lowercase hexadecimal
//! spelling, so a file only answers to the handle it was minted with. The session identifier is
//! recorded beside it as provenance and is deliberately *not* the thing resolution checks — a
//! gateway may run every meeting under the same session identifier.
//!
//! The key is read from an environment variable named by the configuration, once per recording,
//! and must be a 256-bit key written as 64 hexadecimal characters. Requiring the encoding is
//! what makes the published `key_id` safe: it is an unsalted digest, so a low-entropy value
//! there would be an offline oracle for the key. The key is never stored in a summary, an
//! event, an error, a path or a log line.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use conversation_model_adapters::{
    AudioFrame, CaptureClockPair, CaptureDiscontinuityReason, CaptureDomainHistory, CaptureEpoch,
    CaptureFinishBoundary, CaptureMetadata, FrameProvenance, PcmFormat, PcmSampleFormat,
    CAPTURE_TIMING_BYTES, CLOCK_PAIR_CADENCE_NS, MAX_CLOCK_BRACKET_NS, MAX_CLOCK_PAIR_GAP_NS,
    MAX_CLOCK_RESIDUAL_NS, MAX_RECORDING_TIMING_BYTES, MAX_RECORDING_TIMING_RECORDS,
};
use conversation_protocol::{
    FinalizedRecordingCapability, RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId,
};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::digest::{digest, Context as DigestContext, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hkdf, hmac};
use tokio_util::sync::CancellationToken;

/// The reason recorded for a gap opened by a break in the capture bridge.
pub const CAPTURE_DISCONTINUITY_REASON: &str = "capture_discontinuity";
/// The recording stopped while the live session continued after its sink failed.
pub const RECORDING_FAILURE_REASON: &str = "recording_failed";
/// The reported format of a recording that captured no audio at all.
pub const EMPTY_RECORDING_FORMAT: &str = "none";
/// The most gaps one recording's summary lists. The file records every gap exactly; past this
/// many the summary's last entry is widened to span all later gaps, so memory and the summary
/// stay bounded while the recording itself never stops because of gaps.
pub const MAX_RECORDING_GAPS: usize = 256;

const MAGIC: &[u8; 8] = b"CRSREC02";
const TIMED_MAGIC: &[u8; 8] = b"CRSREC03";
/// The 32-byte layout that had no binding identifier. Refused explicitly: its chunk cursor
/// lands mid-ciphertext under this reader, which fails closed only by probability.
const SUPERSEDED_MAGIC: &[u8; 8] = b"CRSREC01";
const HEADER_LEN: usize = 48;
const AEAD_AES_256_GCM: u8 = 1;
/// The per-recording random identifier. The handle is its hexadecimal spelling, and the header
/// carries the bytes, so resolution can prove a file answers to the handle it is filed under.
const BINDING_BYTES: usize = 16;
const HANDLE_LEN: usize = BINDING_BYTES * 2;
const NONCE_PREFIX_LEN: usize = 8;
/// A recording key is 256 bits written as hexadecimal.
const RECORDING_KEY_HEX_LEN: usize = 64;
const RECORDING_EXTENSION: &str = "crsr";
const MAX_CHUNK_CIPHERTEXT_BYTES: usize = 128 * 1024;
const MAX_KEY_ENV_BYTES: usize = 128;

const CHUNK_FRAME: u8 = 1;
const CHUNK_GAP_STARTED: u8 = 2;
const CHUNK_GAP_ENDED: u8 = 3;
const CHUNK_ENDED: u8 = 4;
const CHUNK_POLICY: u8 = 9;
const CHUNK_TIMED_FRAME: u8 = 10;
const CHUNK_EPOCH: u8 = 11;
const CHUNK_CLOCK_PAIR: u8 = 12;
const CHUNK_TIMING_DISCONTINUITY: u8 = 13;
const CHUNK_ALIGNMENT_LIMIT: u8 = 14;

const SAMPLE_FORMAT_S16LE: u8 = 1;
const SAMPLE_FORMAT_F32LE: u8 = 2;

const KEY_DOMAIN: &[u8] = b"conversation-runtime/session-recording/key/v1\0";
const KEY_ID_DOMAIN: &[u8] = b"conversation-runtime/session-recording/key-id/v1\0";
const CAPABILITY_DOMAIN: &[u8] = b"conversation-runtime/finalized-recording-capability/v1\0";
const DIGEST_BUFFER_BYTES: usize = 64 * 1024;
const MAX_FINALIZED_RECORDING_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Where recordings are written and which environment variable names the key.
///
/// The key itself is never held here — only the name of the variable that holds it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingSettings {
    directory: PathBuf,
    key_env: String,
    #[cfg(feature = "recording-test-support")]
    sink_fault: Option<RecordingSinkFault>,
}

/// The encrypted sink operation an integration test can make fail once.
///
/// This exists behind `recording-test-support`, which the gateway enables only for its test
/// target. It is intentionally not a runtime configuration option.
#[cfg(feature = "recording-test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingSinkFault {
    Write,
    Flush,
    WriteAfterSuccessful(u32),
    FlushAfterSuccessful(u32),
}

impl RecordingSettings {
    pub fn new(
        directory: impl Into<PathBuf>,
        key_env: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let directory = directory.into();
        let key_env = key_env.into();
        if !directory.is_absolute() {
            return Err(configuration_error("recording directory must be absolute"));
        }
        if !is_environment_name(&key_env) {
            return Err(configuration_error(
                "recording key variable name is not a valid environment name",
            ));
        }
        Ok(Self {
            directory,
            key_env,
            #[cfg(feature = "recording-test-support")]
            sink_fault: None,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn key_env(&self) -> &str {
        &self.key_env
    }

    /// Makes the next encrypted chunk write or flush fail in an integration test.
    #[cfg(feature = "recording-test-support")]
    pub fn with_test_sink_fault(mut self, fault: RecordingSinkFault) -> Self {
        self.sink_fault = Some(fault);
        self
    }
}

trait RecordingSink: Send {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    fn sync_all(&mut self) -> std::io::Result<()>;
}

impl RecordingSink for File {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Write::write_all(self, bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(self)
    }

    fn sync_all(&mut self) -> std::io::Result<()> {
        File::sync_all(self)
    }
}

#[cfg(feature = "recording-test-support")]
struct FaultingRecordingSink {
    file: File,
    fault: RecordingSinkFault,
    faulted: bool,
    successful_writes: u32,
    successful_flushes: u32,
}

#[cfg(feature = "recording-test-support")]
impl FaultingRecordingSink {
    fn fail_once(&mut self, operation: RecordingSinkFault) -> std::io::Result<()> {
        let should_fail = match (self.fault, operation) {
            (RecordingSinkFault::Write, RecordingSinkFault::Write)
            | (RecordingSinkFault::Flush, RecordingSinkFault::Flush) => true,
            (RecordingSinkFault::WriteAfterSuccessful(expected), RecordingSinkFault::Write) => {
                self.successful_writes == expected
            }
            (RecordingSinkFault::FlushAfterSuccessful(expected), RecordingSinkFault::Flush) => {
                self.successful_flushes == expected
            }
            _ => false,
        };
        if !self.faulted && should_fail {
            self.faulted = true;
            return Err(std::io::Error::other("test recording sink fault"));
        }
        match operation {
            RecordingSinkFault::Write => self.successful_writes += 1,
            RecordingSinkFault::Flush => self.successful_flushes += 1,
            RecordingSinkFault::WriteAfterSuccessful(_)
            | RecordingSinkFault::FlushAfterSuccessful(_) => unreachable!("operation kind"),
        }
        Ok(())
    }
}

#[cfg(feature = "recording-test-support")]
impl RecordingSink for FaultingRecordingSink {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.fail_once(RecordingSinkFault::Write)?;
        Write::write_all(&mut self.file, bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.fail_once(RecordingSinkFault::Flush)?;
        Write::flush(&mut self.file)
    }

    fn sync_all(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }
}

/// A gap in the recorded media. `ended_at_ms` is absent while the gap is still open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingGap {
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub reason: String,
}

/// What a finalized recording is, stated without any key material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingSummary {
    pub recording_handle: String,
    pub encrypted_path: PathBuf,
    pub sha256: String,
    pub duration_ms: u64,
    pub format: String,
    pub key_id: String,
    pub gaps: Vec<RecordingGap>,
}

/// One decrypted record from a recording.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordedChunk {
    TimedFrame {
        at_ms: u64,
        format: PcmFormat,
        timing: Option<FrameProvenance>,
        bytes: Vec<u8>,
    },
    TimingPolicy,
    CaptureMetadata(CaptureMetadata),
    AlignmentLimit,
    Frame {
        at_ms: u64,
        sequence: u64,
        sample_rate_hz: u32,
        channels: u16,
        sample_format: PcmSampleFormat,
        bytes: Vec<u8>,
    },
    GapStarted {
        at_ms: u64,
        reason: String,
    },
    GapEnded {
        at_ms: u64,
    },
    Ended {
        at_ms: u64,
    },
}

/// A decrypted recording. `truncated` reports a partial trailing chunk, which is what a crash
/// mid-write leaves behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedSession {
    pub session_id: SessionId,
    pub chunks: Vec<RecordedChunk>,
    pub truncated: bool,
}

/// The AEAD key for one recording, derived from the environment variable's value.
struct RecordingKey {
    key: LessSafeKey,
    key_id: String,
    capability_key: hmac::Key,
}

impl fmt::Debug for RecordingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl RecordingKey {
    /// Reads the named variable and derives the key from it.
    ///
    /// The variable must hold a 256-bit key written as 64 hexadecimal characters. That is not
    /// housekeeping: `key_id` is an unsalted digest of this value and is published to every
    /// client, so a guessable value there would be an offline oracle for the key. Requiring the
    /// encoding removes the guessable case rather than trying to detect it.
    ///
    /// The value is never bound to a name that outlives this function, never stored, and never
    /// formatted into a diagnostic — only the variable's name and the expected encoding are.
    fn from_environment(key_env: &str) -> Result<Self, RuntimeError> {
        if !is_environment_name(key_env) {
            return Err(configuration_error(
                "recording key variable name is not a valid environment name",
            ));
        }
        let material = std::env::var(key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                configuration_error(format!(
                    "recording key environment variable {key_env} is not set"
                ))
            })?;
        let material = material.trim().to_ascii_lowercase();
        if material.len() != RECORDING_KEY_HEX_LEN || !material.bytes().all(is_lowercase_hex_byte) {
            return Err(configuration_error(format!(
                "recording key environment variable {key_env} must hold a 256-bit key as 64 hexadecimal characters"
            )));
        }
        let derived = derive(KEY_DOMAIN, material.as_bytes());
        let key_id = lowercase_hex(&derive(KEY_ID_DOMAIN, material.as_bytes())[..8]);
        let key = UnboundKey::new(&AES_256_GCM, &derived)
            .map(LessSafeKey::new)
            .map_err(|_| adapter_error("recording key could not be prepared"))?;
        // HKDF consumes canonical root bytes, never the existing AES-derived key.
        let root = decode_hex::<32>(&material).expect("validated hexadecimal root key");
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, CAPABILITY_DOMAIN).extract(&root);
        let info = [CAPABILITY_DOMAIN];
        let okm = prk
            .expand(&info, hkdf::HKDF_SHA256)
            .map_err(|_| adapter_error("recording authorization could not be prepared"))?;
        let mut authorization_key = [0_u8; 32];
        okm.fill(&mut authorization_key)
            .map_err(|_| adapter_error("recording authorization could not be prepared"))?;
        let capability_key = hmac::Key::new(hmac::HMAC_SHA256, &authorization_key);
        Ok(Self {
            key,
            key_id,
            capability_key,
        })
    }

    fn authorize(&self, capability: &mut FinalizedRecordingCapability) -> Result<(), RuntimeError> {
        capability.authorization = "0".repeat(64);
        let fields = capability_fields(capability)?;
        capability.authorization =
            lowercase_hex(hmac::sign(&self.capability_key, &fields).as_ref());
        Ok(())
    }

    fn verify(&self, capability: &FinalizedRecordingCapability) -> Result<(), RuntimeError> {
        let fields = capability_fields(capability)?;
        let tag = decode_hex::<32>(&capability.authorization).ok_or_else(recovery_error)?;
        if capability.key_id != self.key_id {
            return Err(recovery_error());
        }
        hmac::verify(&self.capability_key, &fields, &tag).map_err(|_| recovery_error())
    }

    fn seal(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        plaintext: &mut Vec<u8>,
    ) -> Result<(), RuntimeError> {
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                plaintext,
            )
            .map_err(|_| adapter_error("recording chunk could not be encrypted"))
    }

    fn open(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        ciphertext: &mut [u8],
    ) -> Result<usize, RuntimeError> {
        self.key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                ciphertext,
            )
            .map(|plaintext| plaintext.len())
            .map_err(|_| adapter_error("recording chunk could not be decrypted"))
    }
}

/// Appends encrypted chunks to one session's recording, one capture frame at a time.
pub struct SessionRecorder {
    session_id: SessionId,
    handle: String,
    path: PathBuf,
    key: RecordingKey,
    header: [u8; HEADER_LEN],
    file: Option<Box<dyn RecordingSink>>,
    counter: u32,
    started: Instant,
    format: Option<PcmFormat>,
    gaps: Vec<RecordingGap>,
    /// The open gap's `at_ms`, the very value written into its chunk. Charging the timeline
    /// with the difference of the two recorded timestamps — rather than with a separate
    /// `Instant` measured a file write later — is what makes the file the single source of
    /// truth for the media timeline: a reader that reconstructs the timeline from the chunks
    /// gets exactly the timeline the summary reports, not one a millisecond beside it.
    open_gap_at_ms: Option<u64>,
    /// The length of the media timeline: the audio recorded so far plus the gaps between it.
    /// The recorder's own lifetime is deliberately not part of it, so a client that waits
    /// before finalizing is not told the recording grew.
    media_ms: u64,
    /// Set once the closing chunk is on disk, so a finalize that fails afterwards retries the
    /// failing step instead of appending a second closing chunk or reporting a wrong error.
    ended: bool,
    /// Set once an incomplete recorder has written its failure record, which always precedes
    /// the closing chunk. The file itself then states that it can never authorize recovery.
    failure_recorded: bool,
    summary: Option<RecordingSummary>,
    recording_failed: bool,
    /// Issuance must describe the exact successfully sealed transcript, not bytes replaced
    /// behind the writer or a retry after a partial encrypted write. Legacy salvage is unchanged.
    sealed_digest: DigestContext,
    capability_poisoned: bool,
    timing_budget: Option<TimingBudget>,
    timed_samples: u64,
    timed_gap_ms: u64,
    timed_validation: Option<TimedValidation>,
}

struct TimingBudget {
    records: u64,
    bytes: u64,
    maximum_records: u64,
    maximum_bytes: u64,
    exhausted: bool,
}
impl Default for TimingBudget {
    fn default() -> Self {
        Self {
            records: 0,
            bytes: 0,
            maximum_records: MAX_RECORDING_TIMING_RECORDS,
            maximum_bytes: MAX_RECORDING_TIMING_BYTES,
            exhausted: false,
        }
    }
}

impl fmt::Debug for SessionRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRecorder")
            .field("session_id", &self.session_id)
            .field("handle", &self.handle)
            .field("path", &self.path)
            .field("key_id", &self.key.key_id)
            .finish_non_exhaustive()
    }
}

impl SessionRecorder {
    /// Resolves the key, mints a handle and creates the file. Every failure here — an absent
    /// key variable above all — refuses the recording rather than starting one that cannot be
    /// protected, and leaves nothing on disk.
    pub fn start(
        settings: &RecordingSettings,
        session_id: SessionId,
    ) -> Result<Self, RuntimeError> {
        Self::start_format(settings, session_id, false)
    }

    pub fn start_timed(
        settings: &RecordingSettings,
        session_id: SessionId,
    ) -> Result<Self, RuntimeError> {
        let mut recorder = Self::start_format(settings, session_id, true)?;
        recorder.append(policy_record())?;
        Ok(recorder)
    }

    fn start_format(
        settings: &RecordingSettings,
        session_id: SessionId,
        timed: bool,
    ) -> Result<Self, RuntimeError> {
        let key = RecordingKey::from_environment(&settings.key_env)?;
        let random = SystemRandom::new();
        let mut binding = [0_u8; BINDING_BYTES];
        let mut nonce_prefix = [0_u8; NONCE_PREFIX_LEN];
        random
            .fill(&mut binding)
            .and_then(|()| random.fill(&mut nonce_prefix))
            .map_err(|_| adapter_error("recording randomness was unavailable"))?;
        let handle = lowercase_hex(&binding);

        let mut header = [0_u8; HEADER_LEN];
        header[..8].copy_from_slice(if timed { TIMED_MAGIC } else { MAGIC });
        header[8] = AEAD_AES_256_GCM;
        header[12..20].copy_from_slice(&session_id.get().to_be_bytes());
        header[20..28].copy_from_slice(&nonce_prefix);
        header[28..44].copy_from_slice(&binding);

        std::fs::create_dir_all(&settings.directory)
            .map_err(|_| adapter_error("recording directory could not be created"))?;
        let path = settings
            .directory
            .join(format!("{handle}.{RECORDING_EXTENSION}"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|_| adapter_error("recording file could not be created"))?;
        Write::write_all(&mut file, &header)
            .and_then(|()| Write::flush(&mut file))
            .map_err(|_| adapter_error("recording header could not be written"))?;

        let file: Box<dyn RecordingSink> = {
            #[cfg(feature = "recording-test-support")]
            if let Some(fault) = settings.sink_fault {
                Box::new(FaultingRecordingSink {
                    file,
                    fault,
                    faulted: false,
                    successful_writes: 0,
                    successful_flushes: 0,
                })
            } else {
                Box::new(file)
            }
            #[cfg(not(feature = "recording-test-support"))]
            Box::new(file)
        };

        let mut sealed_digest = DigestContext::new(&SHA256);
        sealed_digest.update(&header);
        Ok(Self {
            session_id,
            handle,
            path,
            key,
            header,
            file: Some(file),
            counter: 0,
            started: Instant::now(),
            format: None,
            gaps: Vec::new(),
            open_gap_at_ms: None,
            media_ms: 0,
            ended: false,
            failure_recorded: false,
            summary: None,
            recording_failed: false,
            sealed_digest,
            capability_poisoned: false,
            timing_budget: timed.then(TimingBudget::default),
            timed_samples: 0,
            timed_gap_ms: 0,
            timed_validation: timed.then(TimedValidation::default),
        })
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn handle(&self) -> &str {
        &self.handle
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the recording is closed and its summary issued.
    pub fn is_finalized(&self) -> bool {
        self.summary.is_some()
    }

    /// Whether a write failed, so the file can never authorize recovery. Plain `finalize` still
    /// closes it with its gap records and integrity digest.
    pub fn is_incomplete(&self) -> bool {
        self.recording_failed || self.capability_poisoned
    }

    /// Marks this recorder incomplete after a sink failure. The caller must stop feeding it;
    /// the final summary retains this bounded fact even when the meeting stays live.
    pub fn mark_incomplete(&mut self) {
        if self.recording_failed {
            return;
        }
        self.recording_failed = true;
        let failure = RecordingGap {
            started_at_ms: self.media_duration_ms(),
            ended_at_ms: None,
            reason: RECORDING_FAILURE_REASON.to_owned(),
        };
        if self.gaps.len() < MAX_RECORDING_GAPS {
            self.gaps.push(failure);
        } else if let Some(last) = self.gaps.last_mut() {
            // The cap itself is a recording failure. Preserve a bounded, explicit failure
            // marker rather than silently discarding the fact because the gap list is full.
            *last = failure;
        }
    }

    /// Records one capture frame. Called for every frame the bridge delivers, whatever the
    /// speech, language and synthesis providers happen to be doing.
    pub fn record_frame(&mut self, frame: &AudioFrame) -> Result<(), RuntimeError> {
        let format = frame.format();
        // Read before the assignment below, or the gap guard can never be false: a gap that
        // opened before any audio arrived would be charged to the timeline as if it sat between
        // two frames.
        let had_media = self.format.is_some();
        match self.format {
            Some(existing) if existing != format => {
                return Err(adapter_error(
                    "recording capture format changed mid-session",
                ));
            }
            Some(_) => {}
            None => self.format = Some(format),
        }
        let at_ms = self.elapsed_ms();
        if let Some(opened_at_ms) = self.open_gap_at_ms {
            self.append(gap_ended_record(at_ms))?;
            self.open_gap_at_ms = None;
            // A gap between two frames is media the recording does not have, so it occupies its
            // real length on the timeline. A gap before any frame extends nothing: the
            // recording simply had not started.
            if had_media {
                if self.timing_budget.is_some() {
                    self.timed_gap_ms = self
                        .timed_gap_ms
                        .checked_add(at_ms.saturating_sub(opened_at_ms))
                        .ok_or_else(recovery_error)?;
                }
                self.media_ms = self
                    .media_ms
                    .saturating_add(at_ms.saturating_sub(opened_at_ms));
            }
            let media_ms = self.media_ms;
            if let Some(gap) = self.gaps.last_mut() {
                gap.ended_at_ms = Some(media_ms);
            }
        }
        if self.timing_budget.is_some() {
            return self.record_timed_frame(frame, at_ms);
        }
        let mut record = Vec::with_capacity(24 + frame.bytes().len());
        record.push(CHUNK_FRAME);
        record.extend_from_slice(&at_ms.to_be_bytes());
        record.extend_from_slice(&frame.sequence().to_be_bytes());
        record.extend_from_slice(&format.sample_rate_hz().to_be_bytes());
        record.extend_from_slice(&format.channels().to_be_bytes());
        record.push(sample_format_code(format.sample_format())?);
        record.extend_from_slice(frame.bytes());
        self.append(record)?;
        self.media_ms = self.media_ms.saturating_add(frame_duration_ms(frame));
        Ok(())
    }

    fn admit_timing(&mut self, bytes: usize) -> Result<bool, RuntimeError> {
        let Some(budget) = self.timing_budget.as_mut() else {
            return Ok(false);
        };
        if budget.exhausted {
            return Ok(false);
        }
        let bytes = u64::try_from(bytes).map_err(|_| recovery_error())?;
        if budget.records >= budget.maximum_records
            || budget
                .bytes
                .checked_add(bytes)
                .is_none_or(|total| total > budget.maximum_bytes)
        {
            budget.exhausted = true;
            self.append(vec![CHUNK_ALIGNMENT_LIMIT])?;
            return Ok(false);
        }
        budget.records += 1;
        budget.bytes += bytes;
        Ok(true)
    }

    pub fn record_capture_metadata(
        &mut self,
        metadata: &CaptureMetadata,
    ) -> Result<(), RuntimeError> {
        if self.timing_budget.is_none() {
            return Err(invalid_state_error(
                "legacy recording does not accept timing",
            ));
        }
        let record = metadata_record(metadata)?;
        if self.admit_timing(record.len() - 1)? {
            self.append(record)?;
        }
        Ok(())
    }

    fn record_timed_frame(&mut self, frame: &AudioFrame, at_ms: u64) -> Result<(), RuntimeError> {
        let timing = frame
            .capture_timing()
            .map(|timing| timing.frame.encode())
            .transpose()
            .map_err(|_| recovery_error())?;
        let timing = if timing.is_some() && self.admit_timing(CAPTURE_TIMING_BYTES)? {
            timing
        } else {
            None
        };
        let format = frame.format();
        let mut record = Vec::with_capacity(17 + CAPTURE_TIMING_BYTES + frame.bytes().len());
        record.push(CHUNK_TIMED_FRAME);
        record.extend_from_slice(&at_ms.to_be_bytes());
        record.push(sample_format_code(format.sample_format())?);
        record.extend_from_slice(&format.sample_rate_hz().to_be_bytes());
        record.extend_from_slice(&format.channels().to_be_bytes());
        record.push(u8::from(timing.is_some()));
        if let Some(timing) = timing {
            record.extend_from_slice(&timing);
        }
        record.extend_from_slice(frame.bytes());
        let samples = u64::try_from(
            frame.bytes().len()
                / format
                    .frame_alignment_bytes()
                    .map_err(|_| recovery_error())?,
        )
        .map_err(|_| recovery_error())?;
        let total = self
            .timed_samples
            .checked_add(samples)
            .ok_or_else(recovery_error)?;
        let duration = exact_media_ms(total, format.sample_rate_hz(), self.timed_gap_ms)?;
        self.append(record)?;
        self.timed_samples = total;
        self.media_ms = duration;
        Ok(())
    }

    /// Records a break in the capture bridge as a gap. The next frame closes it, so the
    /// recording states the silence instead of manufacturing it.
    pub fn record_capture_gap(&mut self) -> Result<(), RuntimeError> {
        if self.open_gap_at_ms.is_some() {
            return Ok(());
        }
        let at_ms = self.elapsed_ms();
        let reason = CAPTURE_DISCONTINUITY_REASON;
        self.append(gap_started_record(at_ms, reason))?;
        self.open_gap_at_ms = Some(at_ms);
        if self.gaps.len() < MAX_RECORDING_GAPS {
            self.gaps.push(RecordingGap {
                started_at_ms: self.media_ms,
                ended_at_ms: None,
                reason: reason.to_owned(),
            });
        } else if let Some(last) = self.gaps.last_mut() {
            // Reopened, not replaced: the closing frame widens it to cover this gap too.
            last.ended_at_ms = None;
        }
        Ok(())
    }

    /// Confirms and flushes the exact final timed-capture boundary without appending data.
    pub fn confirm_capture_boundary(
        &mut self,
        boundary: &CaptureFinishBoundary,
    ) -> Result<(), RuntimeError> {
        let validation = self.timed_validation.as_ref().ok_or_else(recovery_error)?;
        if boundary.session_id() != self.session_id
            || self.recording_failed
            || self.capability_poisoned
            || self.ended
            || self.summary.is_some()
            || self.file.is_none()
            || self
                .timing_budget
                .as_ref()
                .is_none_or(|budget| budget.exhausted)
            || validation.exhausted
            || validation
                .epoch
                .as_ref()
                .is_none_or(|epoch| epoch.capture_epoch != boundary.capture_epoch())
            || validation.frame_boundary() != boundary.frame()
            || validation
                .pair
                .as_ref()
                .is_none_or(|pair| pair.sequence != boundary.final_clock_pair_sequence())
        {
            return Err(recovery_error());
        }
        self.capability_poisoned = true;
        self.file
            .as_mut()
            .expect("checked recording sink")
            .flush()
            .map_err(|_| adapter_error("recording file could not be flushed"))?;
        self.capability_poisoned = false;
        Ok(())
    }

    /// Closes the recording and states what it is.
    ///
    /// Idempotent in both directions: a second call after success returns the same summary and
    /// appends nothing, and a second call after a failure retries the step that failed rather
    /// than reporting the recorder as already closed. Nothing is torn down until every step has
    /// succeeded, so a caller that retries always sees the real error.
    pub fn finalize(&mut self) -> Result<RecordingSummary, RuntimeError> {
        self.finalize_using(None)
    }

    fn finalize_using(
        &mut self,
        recovery_source: Option<File>,
    ) -> Result<RecordingSummary, RuntimeError> {
        if let Some(summary) = self.summary.as_ref() {
            return Ok(summary.clone());
        }
        if !self.ended {
            // A recorder that can never be issued a capability says so inside the file, before
            // the closing chunk, so a closed file cannot later be adopted as complete.
            if self.is_incomplete() && !self.failure_recorded {
                self.append(gap_started_record(
                    self.elapsed_ms(),
                    RECORDING_FAILURE_REASON,
                ))?;
                self.failure_recorded = true;
            }
            self.append(ended_record(self.elapsed_ms()))?;
            self.ended = true;
        }
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .and_then(|()| file.sync_all())
                .map_err(|_| adapter_error("recording file could not be flushed"))?;
        }
        let sha256 = if let Some(mut source) = recovery_source {
            // The v4 path retains its admitted descriptor through flush/sync and hashing;
            // reopening the pathname here would reintroduce special-file replacement races.
            let size = admitted_recovery_extent(&source)?;
            source
                .seek(SeekFrom::Start(0))
                .map_err(|_| recovery_error())?;
            let hash = stream_digest(
                &mut (&mut source).take(size),
                None,
                &CancellationToken::new(),
            )?;
            let mut extra = [0];
            if source.read(&mut extra).map_err(|_| recovery_error())? != 0 {
                return Err(recovery_error());
            }
            hash
        } else {
            file_digest(&self.path)?
        };
        let summary = RecordingSummary {
            recording_handle: self.handle.clone(),
            encrypted_path: self.path.clone(),
            sha256,
            duration_ms: self.media_duration_ms(),
            format: format_label(self.format),
            key_id: self.key.key_id.clone(),
            gaps: self.reported_gaps(),
        };
        self.file = None;
        self.summary = Some(summary.clone());
        Ok(summary)
    }

    /// Finalizes durably before issuing a restart capability. The existing v3 summary and
    /// encrypted layout are unchanged. An incomplete sink cannot authorize recovery.
    pub fn finalize_with_capability(
        &mut self,
    ) -> Result<(RecordingSummary, FinalizedRecordingCapability), RuntimeError> {
        if self.recording_failed || self.capability_poisoned {
            return Err(recovery_error());
        }
        // Also admit/check cached finalizations: never knowingly issue an oversized or
        // special-file capability. Legacy finalize deliberately retains its old policy.
        let source = open_recovery_source(&self.path)?;
        let summary = self.finalize_using(Some(source))?;
        if summary.sha256 != lowercase_hex(self.sealed_digest.clone().finish().as_ref()) {
            return Err(recovery_error());
        }
        let mut capability = FinalizedRecordingCapability {
            source_session_id: self.session_id,
            recording_handle: summary.recording_handle.clone(),
            sha256: summary.sha256.clone(),
            key_id: summary.key_id.clone(),
            authorization: String::new(),
        };
        self.key.authorize(&mut capability)?;
        Ok((summary, capability))
    }

    /// The recording's length: the audio it holds plus the gaps between that audio. Zero when
    /// no frame was ever recorded, whatever the session cost in wall-clock time.
    fn media_duration_ms(&self) -> u64 {
        if self.format.is_some() {
            self.media_ms
        } else {
            0
        }
    }

    /// The gaps as the summary states them. They already sit on the media timeline; the clamp
    /// only catches the degenerate case of a recording that captured no audio at all, where the
    /// timeline has no length for a gap to occupy. The file keeps the recorder-relative
    /// timestamps for a consumer that wants arrival times.
    fn reported_gaps(&self) -> Vec<RecordingGap> {
        clamped_gaps(&self.gaps, self.media_duration_ms())
    }

    fn elapsed_ms(&self) -> u64 {
        instant_elapsed_ms(self.started)
    }

    fn append(&mut self, mut record: Vec<u8>) -> Result<(), RuntimeError> {
        let mut timing_state = self.timed_validation.clone();
        if let Some(state) = timing_state.as_mut() {
            state.accept(&decode_chunk(&record)?)?;
        }
        let Some(file) = self.file.as_mut() else {
            return Err(invalid_state_error("recording is already finalized"));
        };
        let previously_poisoned = self.capability_poisoned;
        self.capability_poisoned = true;
        let counter = self.counter;
        self.counter = counter
            .checked_add(1)
            .ok_or_else(|| adapter_error("recording exceeded its chunk limit"))?;
        self.key.seal(
            nonce(&self.header, counter),
            &aad(&self.header, counter),
            &mut record,
        )?;
        let length = u32::try_from(record.len())
            .ok()
            .filter(|length| *length as usize <= MAX_CHUNK_CIPHERTEXT_BYTES)
            .ok_or_else(|| adapter_error("recording chunk exceeded its size limit"))?;
        file.write_all(&length.to_be_bytes())
            .and_then(|()| file.write_all(&record))
            .and_then(|()| file.flush())
            .map_err(|_| adapter_error("recording chunk could not be written"))?;
        self.sealed_digest.update(&length.to_be_bytes());
        self.sealed_digest.update(&record);
        self.capability_poisoned = previously_poisoned;
        self.timed_validation = timing_state;
        Ok(())
    }
}

/// Resolves a recording handle to its file, refusing a file that answers to a different handle.
///
/// The check is the header's per-recording binding identifier, of which the handle is the
/// hexadecimal spelling. It is deliberately *not* the session identifier: a gateway may run
/// every meeting under the same one, so a session check would let any recording resolve any
/// other recording's handle. Nothing here needs the key, which is why the Meeting Agent can
/// copy a recording by handle without ever holding one.
pub fn resolve_recording_handle(directory: &Path, handle: &str) -> Result<PathBuf, RuntimeError> {
    let Some(binding) = decode_handle(handle) else {
        return Err(configuration_error(
            "recording handle is not a valid handle",
        ));
    };
    let path = directory.join(format!("{handle}.{RECORDING_EXTENSION}"));
    let mut file = File::open(&path).map_err(|_| adapter_error("recording could not be opened"))?;
    let mut header = [0_u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|_| adapter_error("recording header could not be read"))?;
    read_header(&header)?;
    if header[28..44] != binding {
        return Err(invalid_state_error(
            "recording does not answer to that handle",
        ));
    }
    Ok(path)
}

/// The 16 binding bytes a handle spells, or `None` when it is not a handle at all. The charset
/// and length check is also the path-traversal guard.
fn decode_handle(handle: &str) -> Option<[u8; BINDING_BYTES]> {
    if handle.len() != HANDLE_LEN || !handle.bytes().all(is_lowercase_hex_byte) {
        return None;
    }
    let mut binding = [0_u8; BINDING_BYTES];
    for (byte, pair) in binding.iter_mut().zip(handle.as_bytes().chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(high * 16 + low).ok()?;
    }
    Some(binding)
}

/// Walks a recording's chunks one at a time, decrypting each as it is read.
///
/// This is the reader a post-meeting reconciliation uses. A two-hour meeting is roughly
/// 230 MB, so holding the whole file — which is what [`read_session_recording`] does — is not
/// an option there. Both readers validate the header through the one [`read_header`] function,
/// so neither can accept a layout the other refuses.
pub struct RecordedChunks {
    reader: BufReader<PositionedFile>,
    key: Arc<RecordingKey>,
    header: [u8; HEADER_LEN],
    session_id: SessionId,
    counter: u32,
    truncated: bool,
    finished: bool,
    strict: bool,
    // Drops after the reader, including all early error and cancellation paths.
    _snapshot: Option<PrivateSnapshot>,
}

/// Opens a recording and validates its header, without reading a single chunk.
pub fn open_session_recording(path: &Path, key_env: &str) -> Result<RecordedChunks, RuntimeError> {
    let key = RecordingKey::from_environment(key_env)?;
    let mut file = File::open(path).map_err(|_| adapter_error("recording could not be opened"))?;
    let mut header = [0_u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|_| adapter_error("recording header is incomplete"))?;
    let session_id = read_header(&header)?;
    Ok(RecordedChunks {
        reader: BufReader::new(PositionedFile::new(file, HEADER_LEN as u64)),
        key: Arc::new(key),
        header,
        session_id,
        counter: 0,
        truncated: false,
        finished: false,
        strict: false,
        _snapshot: None,
    })
}

impl RecordedChunks {
    pub fn is_timed(&self) -> bool {
        &self.header[..8] == TIMED_MAGIC
    }

    /// Exactly two logical cursors over the retained authenticated snapshot.
    /// Neither cursor reopens a pathname or shares the other's seek position.
    pub(crate) fn into_alignment_cursors(self) -> Result<(Self, Self), RuntimeError> {
        if !self.strict || self._snapshot.is_none() || self.counter != 0 {
            return Err(recovery_error());
        }
        let other = Self {
            reader: BufReader::new(PositionedFile {
                file: Arc::clone(&self.reader.get_ref().file),
                offset: HEADER_LEN as u64,
            }),
            key: Arc::clone(&self.key),
            header: self.header,
            session_id: self.session_id,
            counter: 0,
            truncated: false,
            finished: false,
            strict: true,
            _snapshot: None,
        };
        Ok((self, other))
    }
    /// The session that made this recording, as the header states it. Provenance only: what
    /// proves a file answers to a handle is the binding identifier, not this.
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Whether the walk stopped on a partial trailing chunk. Only meaningful once the walk has
    /// ended, which is what a `None` from [`Self::next_chunk`] reports.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// The next whole chunk, or `None` at the end of the recording.
    ///
    /// A partial trailing chunk — what a crash mid-write leaves — ends the walk and sets
    /// `truncated`. A *whole* chunk that fails to open is not a crash but tampering, and fails.
    pub fn next_chunk(&mut self) -> Result<Option<RecordedChunk>, RuntimeError> {
        if self.finished {
            return Ok(None);
        }
        let mut prefix = [0_u8; 4];
        match self.fill(&mut prefix)? {
            0 => {
                self.finished = true;
                return Ok(None);
            }
            4 => {}
            _ => return Ok(self.tear()),
        }
        let length = usize::try_from(u32::from_be_bytes(prefix))
            .map_err(|_| adapter_error("recording chunk length is out of range"))?;
        if length > MAX_CHUNK_CIPHERTEXT_BYTES {
            return Err(adapter_error("recording chunk exceeded its size limit"));
        }
        let mut buffer = vec![0_u8; length];
        if self.fill(&mut buffer)? != length {
            return Ok(self.tear());
        }
        let counter = self.counter;
        let plaintext = self.key.open(
            nonce(&self.header, counter),
            &aad(&self.header, counter),
            &mut buffer,
        )?;
        let record = &buffer[..plaintext];
        let kind = record.first().copied().ok_or_else(recovery_error)?;
        if (self.is_timed() && kind == CHUNK_FRAME) || (!self.is_timed() && kind >= CHUNK_POLICY) {
            return Err(recovery_error());
        }
        if self.strict
            && record.first() == Some(&CHUNK_GAP_STARTED)
            && (record.len() < 10 || record.len() != 10 + usize::from(record[9]))
        {
            return Err(recovery_error());
        }
        let chunk = decode_chunk(record)?;
        self.counter = counter
            .checked_add(1)
            .ok_or_else(|| adapter_error("recording exceeded its chunk limit"))?;
        Ok(Some(chunk))
    }

    fn tear(&mut self) -> Option<RecordedChunk> {
        self.finished = true;
        self.truncated = true;
        None
    }

    /// Reads until `buffer` is full or the file ends, reporting how many bytes arrived. A short
    /// count is a tear rather than an error, which is what makes a crashed recording readable.
    fn fill(&mut self, buffer: &mut [u8]) -> Result<usize, RuntimeError> {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = self
                .reader
                .read(&mut buffer[filled..])
                .map_err(|_| adapter_error("recording could not be read"))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        Ok(filled)
    }
}

/// Decrypts a whole recording with the key named by `key_env`.
///
/// Holds the entire decrypted recording in memory, so it suits a short session and a test, not
/// a two-hour meeting; [`open_session_recording`] streams the same chunks instead. A partial
/// trailing chunk is reported as `truncated`; every whole chunk before it is returned.
pub fn read_session_recording(path: &Path, key_env: &str) -> Result<RecordedSession, RuntimeError> {
    let mut walk = open_session_recording(path, key_env)?;
    let mut chunks = Vec::new();
    while let Some(chunk) = walk.next_chunk()? {
        chunks.push(chunk);
    }
    Ok(RecordedSession {
        session_id: walk.session_id(),
        chunks,
        truncated: walk.truncated(),
    })
}

fn read_header(header: &[u8; HEADER_LEN]) -> Result<SessionId, RuntimeError> {
    if &header[..8] == SUPERSEDED_MAGIC {
        return Err(adapter_error("recording uses a superseded header layout"));
    }
    if (&header[..8] != MAGIC && &header[..8] != TIMED_MAGIC) || header[8] != AEAD_AES_256_GCM {
        return Err(adapter_error("recording header is not recognised"));
    }
    if header[9..12] != [0; 3] || header[44..48] != [0; 4] {
        return Err(adapter_error("recording header reserves are not zero"));
    }
    let mut session = [0_u8; 8];
    session.copy_from_slice(&header[12..20]);
    Ok(SessionId::new(u64::from_be_bytes(session)))
}

fn decode_chunk(record: &[u8]) -> Result<RecordedChunk, RuntimeError> {
    let (kind, body) = record
        .split_first()
        .ok_or_else(|| adapter_error("recording chunk is empty"))?;
    match *kind {
        CHUNK_POLICY if record == policy_record() => Ok(RecordedChunk::TimingPolicy),
        CHUNK_TIMED_FRAME => {
            if body.len() < 16 {
                return Err(recovery_error());
            }
            let at_ms = u64::from_be_bytes(body[..8].try_into().unwrap());
            let format = PcmFormat::new(
                u32::from_be_bytes(body[9..13].try_into().unwrap()),
                u16::from_be_bytes(body[13..15].try_into().unwrap()),
                sample_format_from_code(body[8])?,
            )
            .map_err(|_| recovery_error())?;
            let (timing, offset) = match body[15] {
                0 => (None, 16),
                1 if body.len() >= 16 + CAPTURE_TIMING_BYTES => (
                    Some(
                        FrameProvenance::decode(&body[16..16 + CAPTURE_TIMING_BYTES])
                            .map_err(|_| recovery_error())?,
                    ),
                    16 + CAPTURE_TIMING_BYTES,
                ),
                _ => return Err(recovery_error()),
            };
            let bytes = &body[offset..];
            if bytes.is_empty()
                || bytes.len() > conversation_model_adapters::MAX_PCM_FRAME_BYTES
                || !bytes.len().is_multiple_of(
                    format
                        .frame_alignment_bytes()
                        .map_err(|_| recovery_error())?,
                )
            {
                return Err(recovery_error());
            }
            if let Some(timing) = &timing {
                timing
                    .validate_pcm(format, bytes.len())
                    .map_err(|_| recovery_error())?;
            }
            Ok(RecordedChunk::TimedFrame {
                at_ms,
                format,
                timing,
                bytes: bytes.to_vec(),
            })
        }
        CHUNK_EPOCH => Ok(RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(
            CaptureEpoch::decode(body).map_err(|_| recovery_error())?,
        ))),
        CHUNK_CLOCK_PAIR => Ok(RecordedChunk::CaptureMetadata(CaptureMetadata::ClockPair(
            CaptureClockPair::decode(body).map_err(|_| recovery_error())?,
        ))),
        CHUNK_TIMING_DISCONTINUITY if body.len() == 9 => {
            let capture_epoch = u64::from_be_bytes(body[..8].try_into().unwrap());
            if capture_epoch == 0 {
                return Err(recovery_error());
            }
            Ok(RecordedChunk::CaptureMetadata(
                CaptureMetadata::Discontinuity {
                    capture_epoch,
                    reason: CaptureDiscontinuityReason::decode(body[8])
                        .map_err(|_| recovery_error())?,
                },
            ))
        }
        CHUNK_ALIGNMENT_LIMIT if body.is_empty() => Ok(RecordedChunk::AlignmentLimit),
        CHUNK_FRAME => {
            if body.len() < 23 {
                return Err(adapter_error("recording frame chunk is truncated"));
            }
            let at_ms = u64::from_be_bytes(body[..8].try_into().expect("eight bytes"));
            let sequence = u64::from_be_bytes(body[8..16].try_into().expect("eight bytes"));
            let sample_rate_hz = u32::from_be_bytes(body[16..20].try_into().expect("four bytes"));
            let channels = u16::from_be_bytes(body[20..22].try_into().expect("two bytes"));
            let sample_format = sample_format_from_code(body[22])?;
            Ok(RecordedChunk::Frame {
                at_ms,
                sequence,
                sample_rate_hz,
                channels,
                sample_format,
                bytes: body[23..].to_vec(),
            })
        }
        CHUNK_GAP_STARTED => {
            if body.len() < 9 {
                return Err(adapter_error("recording gap chunk is truncated"));
            }
            let at_ms = u64::from_be_bytes(body[..8].try_into().expect("eight bytes"));
            let length = usize::from(body[8]);
            let reason = body
                .get(9..9 + length)
                .ok_or_else(|| adapter_error("recording gap chunk is truncated"))?;
            Ok(RecordedChunk::GapStarted {
                at_ms,
                reason: String::from_utf8(reason.to_vec())
                    .map_err(|_| adapter_error("recording gap reason is not text"))?,
            })
        }
        CHUNK_GAP_ENDED | CHUNK_ENDED => {
            if body.len() != 8 {
                return Err(adapter_error("recording chunk is truncated"));
            }
            let at_ms = u64::from_be_bytes(body[..8].try_into().expect("eight bytes"));
            Ok(if *kind == CHUNK_GAP_ENDED {
                RecordedChunk::GapEnded { at_ms }
            } else {
                RecordedChunk::Ended { at_ms }
            })
        }
        _ => Err(adapter_error("recording chunk kind is not recognised")),
    }
}

fn format_label(format: Option<PcmFormat>) -> String {
    format.map_or_else(
        || EMPTY_RECORDING_FORMAT.to_owned(),
        |format| {
            format!(
                "{}/{}/{}",
                sample_format_name(format.sample_format()),
                format.sample_rate_hz(),
                format.channels()
            )
        },
    )
}

fn clamped_gaps(gaps: &[RecordingGap], duration: u64) -> Vec<RecordingGap> {
    gaps.iter()
        .map(|gap| RecordingGap {
            started_at_ms: gap.started_at_ms.min(duration),
            ended_at_ms: gap.ended_at_ms.map(|ended| ended.min(duration)),
            reason: gap.reason.clone(),
        })
        .collect()
}

fn gap_started_record(at_ms: u64, reason: &str) -> Vec<u8> {
    let mut record = Vec::with_capacity(10 + reason.len());
    record.push(CHUNK_GAP_STARTED);
    record.extend_from_slice(&at_ms.to_be_bytes());
    record.push(u8::try_from(reason.len()).expect("gap reasons are short"));
    record.extend_from_slice(reason.as_bytes());
    record
}

fn gap_ended_record(at_ms: u64) -> Vec<u8> {
    let mut record = Vec::with_capacity(9);
    record.push(CHUNK_GAP_ENDED);
    record.extend_from_slice(&at_ms.to_be_bytes());
    record
}

fn ended_record(at_ms: u64) -> Vec<u8> {
    let mut record = Vec::with_capacity(9);
    record.push(CHUNK_ENDED);
    record.extend_from_slice(&at_ms.to_be_bytes());
    record
}

fn nonce(header: &[u8; HEADER_LEN], counter: u32) -> [u8; NONCE_LEN] {
    let mut nonce = [0_u8; NONCE_LEN];
    nonce[..NONCE_PREFIX_LEN].copy_from_slice(&header[20..28]);
    nonce[NONCE_PREFIX_LEN..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn aad(header: &[u8; HEADER_LEN], counter: u32) -> [u8; HEADER_LEN + 4] {
    let mut aad = [0_u8; HEADER_LEN + 4];
    aad[..HEADER_LEN].copy_from_slice(header);
    aad[HEADER_LEN..].copy_from_slice(&counter.to_be_bytes());
    aad
}

fn file_digest(path: &Path) -> Result<String, RuntimeError> {
    let mut file = File::open(path).map_err(|_| adapter_error("recording could not be read"))?;
    stream_digest(&mut file, None, &CancellationToken::new())
}

/// Hash exactly the bytes copied, using one fixed-size buffer for finalization and recovery.
fn stream_digest(
    reader: &mut impl Read,
    mut copy: Option<&mut File>,
    cancellation: &CancellationToken,
) -> Result<String, RuntimeError> {
    let mut hash = DigestContext::new(&SHA256);
    let mut buffer = [0_u8; DIGEST_BUFFER_BYTES];
    loop {
        check_recovery_cancellation(cancellation)?;
        let count = reader.read(&mut buffer).map_err(|_| recovery_error())?;
        if count == 0 {
            break;
        }
        if let Some(writer) = copy.as_mut() {
            Write::write_all(writer, &buffer[..count]).map_err(|_| recovery_error())?;
        }
        hash.update(&buffer[..count]);
    }
    Ok(lowercase_hex(hash.finish().as_ref()))
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.bytes().all(is_lowercase_hex_byte) {
        return None;
    }
    let mut result = [0; N];
    for (byte, pair) in result.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *byte = (char::from(pair[0]).to_digit(16)? * 16 + char::from(pair[1]).to_digit(16)?) as u8;
    }
    Some(result)
}

fn capability_fields(capability: &FinalizedRecordingCapability) -> Result<Vec<u8>, RuntimeError> {
    capability.validate().map_err(|_| recovery_error())?;
    let mut fields = CAPABILITY_DOMAIN.to_vec();
    fields.extend_from_slice(&capability.source_session_id.get().to_be_bytes());
    fields.extend_from_slice(
        &decode_hex::<16>(&capability.recording_handle).ok_or_else(recovery_error)?,
    );
    fields.extend_from_slice(&decode_hex::<32>(&capability.sha256).ok_or_else(recovery_error)?);
    fields.extend_from_slice(&decode_hex::<8>(&capability.key_id).ok_or_else(recovery_error)?);
    Ok(fields)
}

fn recovery_error() -> RuntimeError {
    invalid_state_error("finalized recording authorization or content is invalid")
}
fn check_recovery_cancellation(cancellation: &CancellationToken) -> Result<(), RuntimeError> {
    if cancellation.is_cancelled() {
        Err(invalid_state_error("recording recovery was cancelled"))
    } else {
        Ok(())
    }
}

struct PrivateSnapshot(Option<PathBuf>);
impl Drop for PrivateSnapshot {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn open_recovery_source(path: &Path) -> Result<File, RuntimeError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself, never its target; admission below checks
        // the opened handle. Keep sharing compatible with the live finalization sink.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(not(any(unix, windows)))]
    return Err(recovery_error());
    let source = options.open(path).map_err(|_| recovery_error())?;
    admitted_recovery_extent(&source)?;
    Ok(source)
}

fn admitted_recovery_extent(source: &File) -> Result<u64, RuntimeError> {
    let metadata = source.metadata().map_err(|_| recovery_error())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE. Reject all reparse
        // kinds, not merely symlinks recognized by std's FileType implementation.
        if metadata.file_attributes() & (0x0400 | 0x0040) != 0 {
            return Err(recovery_error());
        }
    }
    if !metadata.is_file() || metadata.len() > MAX_FINALIZED_RECORDING_BYTES {
        return Err(recovery_error());
    }
    Ok(metadata.len())
}

/// Keep an immutable-to-the-source encrypted snapshot: descriptor retention alone does not
/// protect against writes to the original inode. No plaintext is ever persisted here.
pub(crate) fn preflight_finalized_recording(
    settings: &RecordingSettings,
    capability: &FinalizedRecordingCapability,
    cancellation: &CancellationToken,
) -> Result<RecordedChunks, RuntimeError> {
    let mut chunks = snapshot_finalized_recording(settings, capability, cancellation)?;
    validate_completed_chunks(&mut chunks, cancellation)?;
    rewind_verified_snapshot(&mut chunks, &capability.sha256, cancellation)?;
    Ok(chunks)
}

/// Verifies the snapshot itself too, not just the bytes observed while copying the original,
/// and rewinds it to the first chunk.
fn rewind_verified_snapshot(
    chunks: &mut RecordedChunks,
    sha256: &str,
    cancellation: &CancellationToken,
) -> Result<(), RuntimeError> {
    chunks
        .reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_error())?;
    if stream_digest(&mut chunks.reader, None, cancellation)? != sha256 {
        return Err(recovery_error());
    }
    chunks
        .reader
        .seek(SeekFrom::Start(HEADER_LEN as u64))
        .map_err(|_| recovery_error())?;
    chunks.counter = 0;
    chunks.finished = false;
    chunks.truncated = false;
    check_recovery_cancellation(cancellation)
}

fn snapshot_finalized_recording(
    settings: &RecordingSettings,
    capability: &FinalizedRecordingCapability,
    cancellation: &CancellationToken,
) -> Result<RecordedChunks, RuntimeError> {
    check_recovery_cancellation(cancellation)?;
    let key = RecordingKey::from_environment(settings.key_env())?;
    key.verify(capability)?;
    let binding = decode_handle(&capability.recording_handle).ok_or_else(recovery_error)?;
    let (chunks, sha256) =
        snapshot_recording(settings, key, &capability.recording_handle, cancellation)?;
    if sha256 != capability.sha256
        || chunks.session_id != capability.source_session_id
        || chunks.header[28..44] != binding
    {
        return Err(recovery_error());
    }
    Ok(chunks)
}

/// Copies the canonical file for `handle` into a private, already-unlinked snapshot and
/// returns a strict reader over it plus the digest of exactly the bytes copied. The header is
/// structurally validated; which identity it must carry is the caller's check.
fn snapshot_recording(
    settings: &RecordingSettings,
    key: RecordingKey,
    handle: &str,
    cancellation: &CancellationToken,
) -> Result<(RecordedChunks, String), RuntimeError> {
    let path = settings
        .directory()
        .join(format!("{handle}.{RECORDING_EXTENSION}"));
    let mut source = open_recovery_source(&path)?;
    let size = admitted_recovery_extent(&source)?;
    #[cfg(test)]
    recovery_tests::observe_snapshot_allocation(&path)?;
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| recovery_error())?;
    let snapshot_path = settings
        .directory()
        .join(format!(".recovery-{}", lowercase_hex(&random)));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_DELETE_ON_CLOSE: the OS owns removal even on process termination.
        options.share_mode(0).custom_flags(0x0400_0000);
    }
    let mut snapshot = options.open(&snapshot_path).map_err(|_| recovery_error())?;
    let mut guard = PrivateSnapshot(Some(snapshot_path));
    // Unlink while the descriptor is owned: a SIGKILL must not strand another encrypted
    // recording outside custody cleanup. Windows' delete-on-close handle provides the same
    // lifetime; unsupported platforms fail closed rather than leaving a named snapshot.
    #[cfg(unix)]
    {
        std::fs::remove_file(guard.0.as_ref().unwrap()).map_err(|_| recovery_error())?;
        guard.0 = None;
    }
    #[cfg(windows)]
    {
        guard.0 = None;
    }
    #[cfg(not(any(unix, windows)))]
    return Err(recovery_error());
    // Bound this copy to the initial extent, even if the original is being appended to.
    let sha256 = stream_digest(
        &mut (&mut source).take(size),
        Some(&mut snapshot),
        cancellation,
    )?;
    let mut extra = [0];
    if source.read(&mut extra).map_err(|_| recovery_error())? != 0 {
        return Err(recovery_error());
    }
    snapshot
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_error())?;
    let mut header = [0; HEADER_LEN];
    snapshot
        .read_exact(&mut header)
        .map_err(|_| recovery_error())?;
    let session_id = read_header(&header)?;
    Ok((
        RecordedChunks {
            reader: BufReader::new(PositionedFile::new(snapshot, HEADER_LEN as u64)),
            key: Arc::new(key),
            header,
            session_id,
            counter: 0,
            truncated: false,
            finished: false,
            strict: true,
            _snapshot: Some(guard),
        },
        sha256,
    ))
}

fn validate_completed_chunks(
    chunks: &mut RecordedChunks,
    cancellation: &CancellationToken,
) -> Result<(), RuntimeError> {
    let walk = walk_completed_chunks(chunks, cancellation)?;
    if walk.incomplete || !walk.ended || chunks.truncated() {
        return Err(recovery_error());
    }
    Ok(())
}

/// What one authenticated walk over a recording established.
struct CompletedWalk {
    /// The closing chunk was read and nothing whole followed it.
    ended: bool,
    /// The recorder wrote its failure record; the walk stops there.
    incomplete: bool,
    timeline: MediaTimeline,
}

/// Authenticates and orders every chunk, rebuilding the media timeline the recorder reported.
/// A chunk after the closing one, a reordered timestamp, a format change or a malformed gap is
/// an error; a missing closing chunk and a torn tail are reported for the caller to judge.
fn walk_completed_chunks(
    chunks: &mut RecordedChunks,
    cancellation: &CancellationToken,
) -> Result<CompletedWalk, RuntimeError> {
    let mut ended = false;
    let mut format = None;
    let mut gap = None;
    let mut last_at_ms = 0;
    let mut timed_validation = TimedValidation::default();
    let timed = chunks.is_timed();
    let mut timeline = MediaTimeline::default();
    loop {
        check_recovery_cancellation(cancellation)?;
        let Some(chunk) = chunks.next_chunk()? else {
            break;
        };
        if ended {
            return Err(recovery_error());
        }
        if timed {
            timed_validation.accept(&chunk)?;
        }
        let at_ms = match chunk {
            RecordedChunk::TimedFrame {
                at_ms,
                format: current,
                bytes,
                ..
            } => {
                if gap.is_some() || format.is_some_and(|prior| prior != current) || bytes.is_empty()
                {
                    return Err(recovery_error());
                }
                format = Some(current);
                timeline.timed_frame(current, bytes.len())?;
                at_ms
            }
            RecordedChunk::TimingPolicy
            | RecordedChunk::CaptureMetadata(_)
            | RecordedChunk::AlignmentLimit => continue,
            RecordedChunk::Frame {
                at_ms,
                sample_rate_hz,
                channels,
                sample_format,
                bytes,
                ..
            } => {
                let current = PcmFormat::new(sample_rate_hz, channels, sample_format)
                    .map_err(|_| recovery_error())?;
                let alignment = current
                    .frame_alignment_bytes()
                    .map_err(|_| recovery_error())?;
                if gap.is_some()
                    || format.is_some_and(|prior| prior != current)
                    || bytes.is_empty()
                    || bytes.len() % alignment != 0
                {
                    return Err(recovery_error());
                }
                format = Some(current);
                timeline.frame(current, bytes.len());
                at_ms
            }
            RecordedChunk::GapStarted { at_ms, reason } => {
                if reason == RECORDING_FAILURE_REASON {
                    return Ok(CompletedWalk {
                        ended: false,
                        incomplete: true,
                        timeline,
                    });
                }
                if gap.is_some() || reason != CAPTURE_DISCONTINUITY_REASON {
                    return Err(recovery_error());
                }
                gap = Some(at_ms);
                timeline.gap_started(at_ms);
                at_ms
            }
            RecordedChunk::GapEnded { at_ms } => {
                if gap.take().is_none() {
                    return Err(recovery_error());
                }
                timeline.gap_ended(at_ms, timed)?;
                at_ms
            }
            // A capture discontinuity may legitimately remain open at finalization.
            RecordedChunk::Ended { at_ms } => {
                ended = true;
                at_ms
            }
        };
        if at_ms < last_at_ms {
            return Err(recovery_error());
        }
        last_at_ms = at_ms;
    }
    Ok(CompletedWalk {
        ended,
        incomplete: false,
        timeline,
    })
}

/// The media timeline rebuilt from a recording's chunks by the same rules the recorder applies
/// while writing them, so a closed file states the summary its recorder would have.
#[derive(Default)]
struct MediaTimeline {
    format: Option<PcmFormat>,
    media_ms: u64,
    timed_samples: u64,
    timed_gap_ms: u64,
    open_gap_at_ms: Option<u64>,
    gaps: Vec<RecordingGap>,
}

impl MediaTimeline {
    fn frame(&mut self, format: PcmFormat, bytes: usize) {
        self.format = Some(format);
        self.media_ms = self.media_ms.saturating_add(pcm_duration_ms(format, bytes));
    }

    fn timed_frame(&mut self, format: PcmFormat, bytes: usize) -> Result<(), RuntimeError> {
        let alignment = format
            .frame_alignment_bytes()
            .map_err(|_| recovery_error())?;
        let samples = u64::try_from(bytes / alignment).map_err(|_| recovery_error())?;
        let total = self
            .timed_samples
            .checked_add(samples)
            .ok_or_else(recovery_error)?;
        self.media_ms = exact_media_ms(total, format.sample_rate_hz(), self.timed_gap_ms)?;
        self.timed_samples = total;
        self.format = Some(format);
        Ok(())
    }

    fn gap_started(&mut self, at_ms: u64) {
        self.open_gap_at_ms = Some(at_ms);
        if self.gaps.len() < MAX_RECORDING_GAPS {
            self.gaps.push(RecordingGap {
                started_at_ms: self.media_ms,
                ended_at_ms: None,
                reason: CAPTURE_DISCONTINUITY_REASON.to_owned(),
            });
        } else if let Some(last) = self.gaps.last_mut() {
            last.ended_at_ms = None;
        }
    }

    fn gap_ended(&mut self, at_ms: u64, timed: bool) -> Result<(), RuntimeError> {
        let Some(opened_at_ms) = self.open_gap_at_ms.take() else {
            return Err(recovery_error());
        };
        if self.format.is_some() {
            let length = at_ms.saturating_sub(opened_at_ms);
            if timed {
                self.timed_gap_ms = self
                    .timed_gap_ms
                    .checked_add(length)
                    .ok_or_else(recovery_error)?;
            }
            self.media_ms = self.media_ms.saturating_add(length);
        }
        let media_ms = self.media_ms;
        if let Some(gap) = self.gaps.last_mut() {
            gap.ended_at_ms = Some(media_ms);
        }
        Ok(())
    }

    fn summary(
        &self,
        settings: &RecordingSettings,
        handle: &str,
        sha256: String,
        key_id: String,
    ) -> RecordingSummary {
        let duration_ms = if self.format.is_some() {
            self.media_ms
        } else {
            0
        };
        RecordingSummary {
            recording_handle: handle.to_owned(),
            encrypted_path: settings
                .directory()
                .join(format!("{handle}.{RECORDING_EXTENSION}")),
            sha256,
            duration_ms,
            format: format_label(self.format),
            key_id,
            gaps: clamped_gaps(&self.gaps, duration_ms),
        }
    }
}

/// Why a closed recording could not be adopted. Every refusal mints nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordingAdoptionError {
    /// No recording is filed under that handle.
    NotFound,
    /// The file is not a regular recording file, or a whole chunk fails to authenticate, or
    /// bytes follow its closing chunk.
    Unreadable,
    /// The file has no closing chunk: it is still being written, or its writer died.
    NotClosed,
    /// The recorder's sink failed, so the file states that it is incomplete.
    Incomplete,
    /// The header names another session, or the file answers to another handle.
    IdentityMismatch,
    /// The key is not configured, or the adoption was cancelled.
    Unavailable(RuntimeError),
}

/// Issues the capability for a recording that was closed on disk without a finalize reply
/// reaching its client — the close a gateway performs when its control input ends.
///
/// The file must answer to `recording_handle`, name `source_session_id` in its header,
/// authenticate chunk by chunk under the recording key, end in exactly one closing chunk and
/// carry no failure record. The summary is rebuilt from the chunks by the recorder's own
/// timeline rules, and the capability covers the same fields under the same key, so both are
/// exactly what a live `finalize_with_capability` of that recording returned or would have
/// returned. Adoption is stateless: the same unchanged file always yields the same answer.
pub fn adopt_closed_recording(
    settings: &RecordingSettings,
    source_session_id: SessionId,
    recording_handle: &str,
    cancellation: &CancellationToken,
) -> Result<(RecordingSummary, FinalizedRecordingCapability), RecordingAdoptionError> {
    use RecordingAdoptionError as Refusal;
    let unavailable_or = |refusal: Refusal| {
        move |error: RuntimeError| {
            if cancellation.is_cancelled() {
                Refusal::Unavailable(error)
            } else {
                refusal
            }
        }
    };
    let binding = decode_handle(recording_handle).ok_or(Refusal::NotFound)?;
    check_recovery_cancellation(cancellation).map_err(Refusal::Unavailable)?;
    let key = RecordingKey::from_environment(settings.key_env()).map_err(Refusal::Unavailable)?;
    let key_id = key.key_id.clone();
    let path = settings
        .directory()
        .join(format!("{recording_handle}.{RECORDING_EXTENSION}"));
    if let Err(error) = std::fs::symlink_metadata(&path) {
        return Err(if error.kind() == std::io::ErrorKind::NotFound {
            Refusal::NotFound
        } else {
            Refusal::Unreadable
        });
    }
    let (mut chunks, sha256) = snapshot_recording(settings, key, recording_handle, cancellation)
        .map_err(unavailable_or(Refusal::Unreadable))?;
    if chunks.header[28..44] != binding || chunks.session_id != source_session_id {
        return Err(Refusal::IdentityMismatch);
    }
    let walk = walk_completed_chunks(&mut chunks, cancellation)
        .map_err(unavailable_or(Refusal::Unreadable))?;
    if walk.incomplete {
        return Err(Refusal::Incomplete);
    }
    if !walk.ended {
        return Err(Refusal::NotClosed);
    }
    if chunks.truncated() {
        return Err(Refusal::Unreadable);
    }
    rewind_verified_snapshot(&mut chunks, &sha256, cancellation)
        .map_err(unavailable_or(Refusal::Unreadable))?;
    let summary = walk
        .timeline
        .summary(settings, recording_handle, sha256, key_id);
    let mut capability = FinalizedRecordingCapability {
        source_session_id: chunks.session_id,
        recording_handle: summary.recording_handle.clone(),
        sha256: summary.sha256.clone(),
        key_id: summary.key_id.clone(),
        authorization: String::new(),
    };
    chunks
        .key
        .authorize(&mut capability)
        .map_err(Refusal::Unavailable)?;
    Ok((summary, capability))
}

#[derive(Clone, Default)]
struct TimedValidation {
    policy: bool,
    exhausted: bool,
    records: u64,
    bytes: u64,
    epoch: Option<CaptureEpoch>,
    domains: CaptureDomainHistory,
    sequence: u64,
    output_end: Option<u64>,
    pair: Option<CaptureClockPair>,
}
impl TimedValidation {
    fn frame_boundary(&self) -> Option<(u64, u64)> {
        self.output_end
            .map(|output_end| (self.sequence, output_end))
    }

    fn accept(&mut self, chunk: &RecordedChunk) -> Result<(), RuntimeError> {
        if !self.policy {
            if !matches!(chunk, RecordedChunk::TimingPolicy) {
                return Err(recovery_error());
            }
            self.policy = true;
            return Ok(());
        }
        let metadata_bytes = match chunk {
            RecordedChunk::TimingPolicy | RecordedChunk::Frame { .. } => {
                return Err(recovery_error())
            }
            RecordedChunk::AlignmentLimit => {
                if self.exhausted {
                    return Err(recovery_error());
                }
                self.exhausted = true;
                return Ok(());
            }
            RecordedChunk::TimedFrame {
                timing: Some(timing),
                ..
            } => {
                let epoch = self.epoch.as_ref().ok_or_else(recovery_error)?;
                if timing.capture_epoch != epoch.capture_epoch
                    || timing.sequence <= self.sequence
                    || self
                        .output_end
                        .is_some_and(|end| timing.output_start_sample < end)
                {
                    return Err(recovery_error());
                }
                self.sequence = timing.sequence;
                self.output_end = Some(
                    timing
                        .output_start_sample
                        .checked_add(u64::from(timing.sample_count))
                        .ok_or_else(recovery_error)?,
                );
                CAPTURE_TIMING_BYTES
            }
            RecordedChunk::CaptureMetadata(metadata) => {
                match metadata {
                    CaptureMetadata::Epoch(epoch) => {
                        self.domains
                            .admit(&epoch.clock_domain_id)
                            .map_err(|_| recovery_error())?;
                        if let Some(prior) = &self.epoch {
                            if prior.bridge_session_id != epoch.bridge_session_id {
                                return Err(recovery_error());
                            }
                            if prior.clock_domain_id == epoch.clock_domain_id {
                                if epoch.capture_epoch < prior.capture_epoch {
                                    return Err(recovery_error());
                                }
                                if epoch.capture_epoch > prior.capture_epoch {
                                    self.sequence = 0;
                                    self.output_end = None;
                                }
                            } else {
                                self.sequence = 0;
                                self.output_end = None;
                                self.pair = None;
                            }
                        }
                        self.epoch = Some(epoch.clone());
                    }
                    CaptureMetadata::ClockPair(pair) => {
                        if self
                            .epoch
                            .as_ref()
                            .is_none_or(|epoch| epoch.clock_domain_id != pair.clock_domain_id)
                        {
                            return Err(recovery_error());
                        }
                        if let Some(prior) = &self.pair {
                            if pair.sequence <= prior.sequence
                                || pair.midpoint_twice() <= prior.midpoint_twice()
                                || pair.wall_epoch < prior.wall_epoch
                            {
                                return Err(recovery_error());
                            }
                        }
                        self.pair = Some(pair.clone());
                    }
                    CaptureMetadata::Discontinuity { capture_epoch, .. } => {
                        if self
                            .epoch
                            .as_ref()
                            .is_none_or(|epoch| epoch.capture_epoch != *capture_epoch)
                        {
                            return Err(recovery_error());
                        }
                    }
                }
                metadata_record(metadata)?.len() - 1
            }
            _ => return Ok(()),
        };
        if self.exhausted {
            return Err(recovery_error());
        }
        self.records = self.records.checked_add(1).ok_or_else(recovery_error)?;
        self.bytes = self
            .bytes
            .checked_add(metadata_bytes as u64)
            .ok_or_else(recovery_error)?;
        if self.records > MAX_RECORDING_TIMING_RECORDS || self.bytes > MAX_RECORDING_TIMING_BYTES {
            return Err(recovery_error());
        }
        Ok(())
    }
}

fn metadata_record(metadata: &CaptureMetadata) -> Result<Vec<u8>, RuntimeError> {
    let (kind, body) = match metadata {
        CaptureMetadata::Epoch(epoch) => {
            (CHUNK_EPOCH, epoch.encode().map_err(|_| recovery_error())?)
        }
        CaptureMetadata::ClockPair(pair) => (
            CHUNK_CLOCK_PAIR,
            pair.encode().map_err(|_| recovery_error())?,
        ),
        CaptureMetadata::Discontinuity {
            capture_epoch,
            reason,
        } => {
            if *capture_epoch == 0 {
                return Err(recovery_error());
            }
            let mut bytes = capture_epoch.to_be_bytes().to_vec();
            bytes.push(reason.code());
            (CHUNK_TIMING_DISCONTINUITY, bytes)
        }
    };
    let mut record = Vec::with_capacity(1 + body.len());
    record.push(kind);
    record.extend_from_slice(&body);
    Ok(record)
}
fn policy_record() -> Vec<u8> {
    let mut record = vec![CHUNK_POLICY];
    for value in [
        CLOCK_PAIR_CADENCE_NS,
        MAX_CLOCK_BRACKET_NS,
        MAX_CLOCK_PAIR_GAP_NS,
        MAX_CLOCK_RESIDUAL_NS,
    ] {
        record.extend_from_slice(&value.to_be_bytes());
    }
    record
}
pub(crate) fn exact_media_ms(samples: u64, rate: u32, gap_ms: u64) -> Result<u64, RuntimeError> {
    if rate == 0 {
        return Err(recovery_error());
    }
    let audio = u128::from(samples) * 1000 / u128::from(rate);
    u64::try_from(audio)
        .ok()
        .and_then(|audio| audio.checked_add(gap_ms))
        .ok_or_else(recovery_error)
}

/// Cursor position belongs to this object. Unix pread/read_at and Windows
/// seek_read use an explicit offset, unlike File::try_clone + ordinary Read.
struct PositionedFile {
    file: Arc<File>,
    offset: u64,
}
impl PositionedFile {
    fn new(file: File, offset: u64) -> Self {
        Self {
            file: Arc::new(file),
            offset,
        }
    }
    fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
}
impl Read for PositionedFile {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            self.file.read_at(bytes, self.offset)?
        };
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(bytes, self.offset)?
        };
        #[cfg(not(any(unix, windows)))]
        let read: usize = return Err(std::io::Error::other(
            "positioned recording reads are unsupported",
        ));
        self.offset = self
            .offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("recording read offset overflowed"))?;
        Ok(read)
    }
}
impl Seek for PositionedFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let offset = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(self.offset) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.metadata()?.len()) + i128::from(delta),
        };
        self.offset = u64::try_from(offset)
            .map_err(|_| std::io::Error::other("recording seek offset is invalid"))?;
        Ok(self.offset)
    }
}

#[cfg(test)]
#[path = "session_recording_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "session_recording_timing_tests.rs"]
mod timing_tests;

#[cfg(test)]
#[path = "session_recording_adoption_tests.rs"]
mod adoption_tests;

fn derive(domain: &[u8], material: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(domain.len() + material.len());
    input.extend_from_slice(domain);
    input.extend_from_slice(material);
    let mut derived = [0_u8; 32];
    derived.copy_from_slice(digest(&SHA256, &input).as_ref());
    derived
}

fn instant_elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How much audio one frame carries, which is what makes the reported duration a media span
/// rather than a wall-clock interval.
fn frame_duration_ms(frame: &AudioFrame) -> u64 {
    pcm_duration_ms(frame.format(), frame.bytes().len())
}

/// How much audio a run of PCM carries. The reconciliation reader accumulates the media
/// timeline the same way the sink did, one frame at a time, so the two agree exactly rather
/// than to within a rounding step.
pub(crate) fn pcm_duration_ms(format: PcmFormat, bytes: usize) -> u64 {
    let Ok(alignment) = format.frame_alignment_bytes() else {
        return 0;
    };
    if alignment == 0 {
        return 0;
    }
    let samples = bytes / alignment;
    u64::try_from(samples as u128 * 1000 / u128::from(format.sample_rate_hz())).unwrap_or(u64::MAX)
}

fn is_lowercase_hex_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble is a hex digit"));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble is a hex digit"));
    }
    hex
}

fn sample_format_code(format: PcmSampleFormat) -> Result<u8, RuntimeError> {
    match format {
        PcmSampleFormat::Signed16LittleEndian => Ok(SAMPLE_FORMAT_S16LE),
        PcmSampleFormat::Float32LittleEndian => Ok(SAMPLE_FORMAT_F32LE),
        _ => Err(adapter_error(
            "recording capture sample format is unsupported",
        )),
    }
}

fn sample_format_from_code(code: u8) -> Result<PcmSampleFormat, RuntimeError> {
    match code {
        SAMPLE_FORMAT_S16LE => Ok(PcmSampleFormat::Signed16LittleEndian),
        SAMPLE_FORMAT_F32LE => Ok(PcmSampleFormat::Float32LittleEndian),
        _ => Err(adapter_error("recording sample format is not recognised")),
    }
}

fn sample_format_name(format: PcmSampleFormat) -> &'static str {
    match format {
        PcmSampleFormat::Signed16LittleEndian => "pcm_s16le",
        PcmSampleFormat::Float32LittleEndian => "pcm_f32le",
        _ => "pcm",
    }
}

fn is_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_KEY_ENV_BYTES
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn configuration_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::AudioCapture,
        message,
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
