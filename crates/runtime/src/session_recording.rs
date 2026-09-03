//! Encrypted session recording.
//!
//! The sink sits on the media path and appends every capture frame the bridge delivers as its
//! own AEAD chunk, so plaintext meeting audio is never written anywhere and a process that dies
//! mid-write still leaves a file that decrypts up to its last whole chunk.
//!
//! # On-disk format (`CRSREC01`)
//!
//! ```text
//! header (48 bytes, plaintext, carries no secret)
//!   0..8    magic  b"CRSREC01"
//!   8       AEAD identifier (1 = AES-256-GCM)
//!   9..12   reserved, zero
//!   12..20  session identifier, u64 big endian (provenance only)
//!   20..28  nonce prefix, 8 random bytes drawn once per recording
//!   28..44  binding identifier, 16 random bytes drawn once per recording
//!   44..48  reserved, zero
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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use conversation_model_adapters::{AudioFrame, PcmFormat, PcmSampleFormat};
use conversation_protocol::{RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

/// The reason recorded for a gap opened by a break in the capture bridge.
pub const CAPTURE_DISCONTINUITY_REASON: &str = "capture_discontinuity";
/// The reported format of a recording that captured no audio at all.
pub const EMPTY_RECORDING_FORMAT: &str = "none";
/// The most gaps one recording may carry. A recording that exceeds it fails closed rather
/// than reporting a truncated gap list.
pub const MAX_RECORDING_GAPS: usize = 256;

const MAGIC: &[u8; 8] = b"CRSREC01";
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

const SAMPLE_FORMAT_S16LE: u8 = 1;
const SAMPLE_FORMAT_F32LE: u8 = 2;

const KEY_DOMAIN: &[u8] = b"conversation-runtime/session-recording/key/v1\0";
const KEY_ID_DOMAIN: &[u8] = b"conversation-runtime/session-recording/key-id/v1\0";

/// Where recordings are written and which environment variable names the key.
///
/// The key itself is never held here — only the name of the variable that holds it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingSettings {
    directory: PathBuf,
    key_env: String,
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
        Ok(Self { directory, key_env })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn key_env(&self) -> &str {
        &self.key_env
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
        Ok(Self { key, key_id })
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
    file: Option<File>,
    counter: u32,
    started: Instant,
    format: Option<PcmFormat>,
    gaps: Vec<RecordingGap>,
    /// When the open gap started, in wall-clock terms, so closing it can extend the media
    /// timeline by however long the bridge was actually silent.
    open_gap: Option<Instant>,
    /// The length of the media timeline: the audio recorded so far plus the gaps between it.
    /// The recorder's own lifetime is deliberately not part of it, so a client that waits
    /// before finalizing is not told the recording grew.
    media_ms: u64,
    /// Set once the closing chunk is on disk, so a finalize that fails afterwards retries the
    /// failing step instead of appending a second closing chunk or reporting a wrong error.
    ended: bool,
    summary: Option<RecordingSummary>,
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
        header[..8].copy_from_slice(MAGIC);
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
        file.write_all(&header)
            .and_then(|()| file.flush())
            .map_err(|_| adapter_error("recording header could not be written"))?;

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
            open_gap: None,
            media_ms: 0,
            ended: false,
            summary: None,
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

    /// Records one capture frame. Called for every frame the bridge delivers, whatever the
    /// speech, language and synthesis providers happen to be doing.
    pub fn record_frame(&mut self, frame: &AudioFrame) -> Result<(), RuntimeError> {
        let format = frame.format();
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
        if let Some(opened_at) = self.open_gap {
            self.append(gap_ended_record(at_ms))?;
            self.open_gap = None;
            // A gap between two frames is media the recording does not have, so it occupies its
            // real length on the timeline. A gap before any frame extends nothing.
            if self.format.is_some() {
                self.media_ms = self.media_ms.saturating_add(instant_elapsed_ms(opened_at));
            }
            let media_ms = self.media_ms;
            if let Some(gap) = self.gaps.last_mut() {
                gap.ended_at_ms = Some(media_ms);
            }
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

    /// Records a break in the capture bridge as a gap. The next frame closes it, so the
    /// recording states the silence instead of manufacturing it.
    pub fn record_capture_gap(&mut self) -> Result<(), RuntimeError> {
        if self.open_gap.is_some() {
            return Ok(());
        }
        if self.gaps.len() >= MAX_RECORDING_GAPS {
            return Err(adapter_error("recording exceeded its gap limit"));
        }
        let at_ms = self.elapsed_ms();
        let reason = CAPTURE_DISCONTINUITY_REASON;
        let mut record = Vec::with_capacity(11 + reason.len());
        record.push(CHUNK_GAP_STARTED);
        record.extend_from_slice(&at_ms.to_be_bytes());
        record.push(u8::try_from(reason.len()).expect("gap reasons are short"));
        record.extend_from_slice(reason.as_bytes());
        self.append(record)?;
        self.open_gap = Some(Instant::now());
        self.gaps.push(RecordingGap {
            started_at_ms: self.media_ms,
            ended_at_ms: None,
            reason: reason.to_owned(),
        });
        Ok(())
    }

    /// Closes the recording and states what it is.
    ///
    /// Idempotent in both directions: a second call after success returns the same summary and
    /// appends nothing, and a second call after a failure retries the step that failed rather
    /// than reporting the recorder as already closed. Nothing is torn down until every step has
    /// succeeded, so a caller that retries always sees the real error.
    pub fn finalize(&mut self) -> Result<RecordingSummary, RuntimeError> {
        if let Some(summary) = self.summary.as_ref() {
            return Ok(summary.clone());
        }
        if !self.ended {
            self.append(ended_record(self.elapsed_ms()))?;
            self.ended = true;
        }
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .and_then(|()| file.sync_all())
                .map_err(|_| adapter_error("recording file could not be flushed"))?;
        }
        let sha256 = file_digest(&self.path)?;
        let summary = RecordingSummary {
            recording_handle: self.handle.clone(),
            encrypted_path: self.path.clone(),
            sha256,
            duration_ms: self.media_duration_ms(),
            format: self.format.map_or_else(
                || EMPTY_RECORDING_FORMAT.to_owned(),
                |format| {
                    format!(
                        "{}/{}/{}",
                        sample_format_name(format.sample_format()),
                        format.sample_rate_hz(),
                        format.channels()
                    )
                },
            ),
            key_id: self.key.key_id.clone(),
            gaps: self.reported_gaps(),
        };
        self.file = None;
        self.summary = Some(summary.clone());
        Ok(summary)
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
        let duration = self.media_duration_ms();
        self.gaps
            .iter()
            .map(|gap| RecordingGap {
                started_at_ms: gap.started_at_ms.min(duration),
                ended_at_ms: gap.ended_at_ms.map(|ended| ended.min(duration)),
                reason: gap.reason.clone(),
            })
            .collect()
    }

    fn elapsed_ms(&self) -> u64 {
        instant_elapsed_ms(self.started)
    }

    fn append(&mut self, mut record: Vec<u8>) -> Result<(), RuntimeError> {
        let Some(file) = self.file.as_mut() else {
            return Err(invalid_state_error("recording is already finalized"));
        };
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
            .map_err(|_| adapter_error("recording chunk could not be written"))
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

/// Decrypts a recording with the key named by `key_env`.
///
/// A partial trailing chunk — what a crash mid-write leaves — stops the read and is reported as
/// `truncated`; every whole chunk before it is returned. A chunk that fails to open is not a
/// crash but tampering, and fails the read.
pub fn read_session_recording(path: &Path, key_env: &str) -> Result<RecordedSession, RuntimeError> {
    let key = RecordingKey::from_environment(key_env)?;
    let raw = std::fs::read(path).map_err(|_| adapter_error("recording could not be read"))?;
    if raw.len() < HEADER_LEN {
        return Err(adapter_error("recording header is incomplete"));
    }
    let mut header = [0_u8; HEADER_LEN];
    header.copy_from_slice(&raw[..HEADER_LEN]);
    let session_id = read_header(&header)?;

    let mut chunks = Vec::new();
    let mut offset = HEADER_LEN;
    let mut counter = 0_u32;
    let mut truncated = false;
    while offset < raw.len() {
        let Some(prefix) = raw.get(offset..offset + 4) else {
            truncated = true;
            break;
        };
        let length = usize::try_from(u32::from_be_bytes([
            prefix[0], prefix[1], prefix[2], prefix[3],
        ]))
        .map_err(|_| adapter_error("recording chunk length is out of range"))?;
        if length > MAX_CHUNK_CIPHERTEXT_BYTES {
            return Err(adapter_error("recording chunk exceeded its size limit"));
        }
        offset += 4;
        let Some(ciphertext) = raw.get(offset..offset + length) else {
            truncated = true;
            break;
        };
        let mut buffer = ciphertext.to_vec();
        let plaintext = key.open(nonce(&header, counter), &aad(&header, counter), &mut buffer)?;
        chunks.push(decode_chunk(&buffer[..plaintext])?);
        offset += length;
        counter = counter
            .checked_add(1)
            .ok_or_else(|| adapter_error("recording exceeded its chunk limit"))?;
    }
    Ok(RecordedSession {
        session_id,
        chunks,
        truncated,
    })
}

fn read_header(header: &[u8; HEADER_LEN]) -> Result<SessionId, RuntimeError> {
    if &header[..8] != MAGIC || header[8] != AEAD_AES_256_GCM {
        return Err(adapter_error("recording header is not recognised"));
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
    let bytes = std::fs::read(path).map_err(|_| adapter_error("recording could not be read"))?;
    Ok(lowercase_hex(digest(&SHA256, &bytes).as_ref()))
}

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
    let format = frame.format();
    let Ok(alignment) = format.frame_alignment_bytes() else {
        return 0;
    };
    if alignment == 0 {
        return 0;
    }
    let samples = frame.bytes().len() / alignment;
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
