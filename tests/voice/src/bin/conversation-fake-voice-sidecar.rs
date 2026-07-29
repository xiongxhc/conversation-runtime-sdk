use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::json;

const PROTOCOL_VERSION: u16 = 1;
const HEADER_BYTES: usize = 8;
const AUDIO_METADATA_BYTES: usize = 48;
const MAX_CONTROL_PAYLOAD_BYTES: usize = 65_536;

const START_SESSION: u16 = 0x0001;
const START_CAPTURE: u16 = 0x0002;
const FLUSH_GENERATION: u16 = 0x0003;
const SHUTDOWN: u16 = 0x0004;
const AUDIO_FRAME: u16 = 0x0100;
const READY: u16 = 0x8001;
const VOICE_ACTIVITY: u16 = 0x8002;
const TRANSCRIPT_HYPOTHESIS: u16 = 0x8003;
const PLAYBACK_ACCEPTED: u16 = 0x8004;
const PLAYBACK_RENDERED: u16 = 0x8005;
const PLAYBACK_FLUSHED: u16 = 0x8006;
const FAILURE: u16 = 0x80fe;
const SHUTDOWN_COMPLETE: u16 = 0x80ff;

const SCENARIO_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO";
const SPAWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SPAWN_MARKER";
const PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_PID_MARKER";
const FLUSH_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_FLUSH_MARKER";
const SHUTDOWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SHUTDOWN_MARKER";
const MEDIA_BLOCKED_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_MEDIA_BLOCKED_MARKER";
const DESCENDANT_PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_DESCENDANT_PID_MARKER";

fn main() {
    let result = run();
    if let Err(error) = result {
        eprintln!("fake-sidecar-error={error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    validate_arguments()?;
    let scenario =
        std::env::var(SCENARIO_ENV).map_err(|_| "missing deterministic scenario".to_owned())?;
    write_marker_from_env(SPAWN_MARKER_ENV, &format!("{}\n", std::process::id()), true)?;
    write_marker_from_env(PID_MARKER_ENV, &std::process::id().to_string(), false)?;

    spawn_stderr_descendant()?;
    if scenario == "crash" {
        eprintln!("fake-sidecar=crash");
        std::process::exit(17);
    }

    let media = File::open("/dev/fd/3").map_err(|error| {
        let descriptors = std::fs::read_dir("/dev/fd")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        format!("failed to open inherited media descriptor: {error}; visible={descriptors:?}")
    })?;

    let (event_sender, event_receiver) = mpsc::channel();
    spawn_control_reader(event_sender.clone(), scenario == "slow-stdin");

    if scenario == "barge-in" {
        write_marker_from_env(MEDIA_BLOCKED_MARKER_ENV, "blocked", false)?;
        thread::spawn(move || {
            let _media = media;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        });
    } else {
        spawn_media_reader(media, event_sender);
    }

    let mut stdout = io::stdout().lock();
    let mut session_id = None;
    let mut ready = false;

    while let Ok(event) = event_receiver.recv() {
        match event {
            FakeEvent::Control(ControlFrame::StartSession { session }) => {
                session_id = Some(session);
                if scenario == "permission-denied" {
                    write_json_frame(
                        &mut stdout,
                        FAILURE,
                        json!({
                            "session_id": session,
                            "stage": "audio_capture",
                            "code": "permission_denied"
                        }),
                    )?;
                    return Ok(());
                }
                write_json_frame(&mut stdout, READY, json!({ "session_id": session }))?;
                ready = true;
                if scenario == "blocked-stdout" {
                    write_header(&mut stdout, VOICE_ACTIVITY, MAX_CONTROL_PAYLOAD_BYTES)?;
                    stdout
                        .flush()
                        .map_err(|_| "failed to flush blocked stdout header".to_owned())?;
                    loop {
                        thread::sleep(Duration::from_secs(60));
                    }
                }
                if scenario == "malformed-frame" {
                    stdout
                        .write_all(&[0, 2, 0, 1, 0, 0, 0, 0])
                        .map_err(|_| "failed to write malformed frame".to_owned())?;
                    stdout
                        .flush()
                        .map_err(|_| "failed to flush malformed frame".to_owned())?;
                }
            }
            FakeEvent::Control(ControlFrame::StartCapture { session }) if ready => {
                if scenario == "partial-final" {
                    write_json_frame(
                        &mut stdout,
                        VOICE_ACTIVITY,
                        json!({
                            "session_id": session,
                            "activity": "speech_started",
                            "at_ms": 10
                        }),
                    )?;
                    write_json_frame(
                        &mut stdout,
                        TRANSCRIPT_HYPOTHESIS,
                        json!({
                            "session_id": session,
                            "segment_id": 4,
                            "text": "hel",
                            "engine_final": false
                        }),
                    )?;
                    write_json_frame(
                        &mut stdout,
                        TRANSCRIPT_HYPOTHESIS,
                        json!({
                            "session_id": session,
                            "segment_id": 4,
                            "text": "hello",
                            "engine_final": true
                        }),
                    )?;
                    write_json_frame(
                        &mut stdout,
                        VOICE_ACTIVITY,
                        json!({
                            "session_id": session,
                            "activity": "speech_ended",
                            "at_ms": 20
                        }),
                    )?;
                } else if scenario == "barge-in" {
                    write_json_frame(
                        &mut stdout,
                        VOICE_ACTIVITY,
                        json!({
                            "session_id": session,
                            "activity": "speech_started",
                            "at_ms": 200
                        }),
                    )?;
                }
            }
            FakeEvent::Control(ControlFrame::FlushGeneration {
                session,
                generation,
            }) if ready => {
                write_marker_from_env(
                    FLUSH_MARKER_ENV,
                    &format!("{session}:{generation}\n"),
                    true,
                )?;
                write_json_frame(
                    &mut stdout,
                    PLAYBACK_FLUSHED,
                    json!({
                        "session_id": session,
                        "generation_id": generation
                    }),
                )?;
            }
            FakeEvent::Control(ControlFrame::Shutdown { session }) if ready => {
                write_marker_from_env(SHUTDOWN_MARKER_ENV, &session.to_string(), false)?;
                write_json_frame(
                    &mut stdout,
                    SHUTDOWN_COMPLETE,
                    json!({ "session_id": session }),
                )?;
                return Ok(());
            }
            FakeEvent::Audio {
                session,
                generation,
            } if ready => {
                let acknowledged_generation = if scenario == "stale-generation" {
                    generation.saturating_sub(1)
                } else {
                    generation
                };
                write_json_frame(
                    &mut stdout,
                    PLAYBACK_ACCEPTED,
                    json!({
                        "session_id": session,
                        "generation_id": acknowledged_generation
                    }),
                )?;
                if scenario != "stale-generation" {
                    write_json_frame(
                        &mut stdout,
                        PLAYBACK_RENDERED,
                        json!({
                            "session_id": session,
                            "generation_id": generation
                        }),
                    )?;
                }
            }
            FakeEvent::Control(ControlFrame::Unexpected)
            | FakeEvent::Control(_)
            | FakeEvent::Audio { .. } => return Err("unexpected fake-sidecar input".to_owned()),
            FakeEvent::Eof => return Ok(()),
            FakeEvent::ReadFailed => return Err("failed to read parent frame".to_owned()),
        }
    }

    if session_id.is_some() {
        Ok(())
    } else {
        Err("control channel closed before session start".to_owned())
    }
}

fn validate_arguments() -> Result<(), String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut model_path = None;
    let mut device = None;
    let mut download = None;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--model-path") => model_path = arguments.next().map(PathBuf::from),
            Some("--device") => device = arguments.next(),
            Some("--download") => download = arguments.next(),
            _ => return Err("unsupported fake-sidecar argument".to_owned()),
        }
    }

    let model_path = model_path.ok_or_else(|| "missing model path".to_owned())?;
    if !model_path.is_absolute() {
        return Err("model path was not absolute".to_owned());
    }
    if device.as_deref() != Some(std::ffi::OsStr::new("system-default")) {
        return Err("device was not system-default".to_owned());
    }
    if download.as_deref() != Some(std::ffi::OsStr::new("false")) {
        return Err("download was not disabled".to_owned());
    }
    Ok(())
}

fn spawn_control_reader(sender: mpsc::Sender<FakeEvent>, stop_after_start: bool) {
    thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin());
        loop {
            match read_frame(&mut stdin) {
                Ok(Some((kind, payload))) => {
                    let control =
                        decode_control(kind, &payload).unwrap_or(ControlFrame::Unexpected);
                    let stop =
                        stop_after_start && matches!(control, ControlFrame::StartSession { .. });
                    if sender.send(FakeEvent::Control(control)).is_err() {
                        return;
                    }
                    if stop {
                        loop {
                            thread::sleep(Duration::from_secs(60));
                        }
                    }
                }
                Ok(None) => {
                    let _ = sender.send(FakeEvent::Eof);
                    return;
                }
                Err(_) => {
                    let _ = sender.send(FakeEvent::ReadFailed);
                    return;
                }
            }
        }
    });
}

fn spawn_media_reader(media: File, sender: mpsc::Sender<FakeEvent>) {
    thread::spawn(move || {
        let mut media = BufReader::new(media);
        loop {
            match read_frame(&mut media) {
                Ok(Some((AUDIO_FRAME, payload))) if payload.len() >= AUDIO_METADATA_BYTES => {
                    let session = read_u64(&payload, 0);
                    let generation = read_u64(&payload, 16);
                    if sender
                        .send(FakeEvent::Audio {
                            session,
                            generation,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => return,
                Ok(Some(_)) | Err(_) => {
                    let _ = sender.send(FakeEvent::ReadFailed);
                    return;
                }
            }
        }
    });
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<(u16, Vec<u8>)>> {
    let mut header = [0_u8; HEADER_BYTES];
    let first = reader.read(&mut header[..1])?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    if u16::from_be_bytes([header[0], header[1]]) != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected protocol version",
        ));
    }
    let kind = u16::from_be_bytes([header[2], header[3]]);
    let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let length = usize::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid payload length"))?;
    if length > MAX_CONTROL_PAYLOAD_BYTES + AUDIO_METADATA_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload exceeded fake limit",
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
}

fn decode_control(kind: u16, payload: &[u8]) -> Option<ControlFrame> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let session = value.get("session_id")?.as_u64()?;
    match kind {
        START_SESSION => Some(ControlFrame::StartSession { session }),
        START_CAPTURE => Some(ControlFrame::StartCapture { session }),
        FLUSH_GENERATION => Some(ControlFrame::FlushGeneration {
            session,
            generation: value.get("generation_id")?.as_u64()?,
        }),
        SHUTDOWN => Some(ControlFrame::Shutdown { session }),
        _ => Some(ControlFrame::Unexpected),
    }
}

fn write_json_frame(
    writer: &mut impl Write,
    kind: u16,
    value: serde_json::Value,
) -> Result<(), String> {
    let payload =
        serde_json::to_vec(&value).map_err(|_| "failed to encode fake frame".to_owned())?;
    write_header(writer, kind, payload.len())?;
    writer
        .write_all(&payload)
        .map_err(|_| "failed to write fake frame payload".to_owned())?;
    writer
        .flush()
        .map_err(|_| "failed to flush fake frame".to_owned())
}

fn write_header(writer: &mut impl Write, kind: u16, length: usize) -> Result<(), String> {
    let length = u32::try_from(length).map_err(|_| "fake payload length overflowed".to_owned())?;
    writer
        .write_all(&PROTOCOL_VERSION.to_be_bytes())
        .and_then(|()| writer.write_all(&kind.to_be_bytes()))
        .and_then(|()| writer.write_all(&length.to_be_bytes()))
        .map_err(|_| "failed to write fake frame header".to_owned())
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(payload[offset..offset + 8].try_into().expect("eight bytes"))
}

fn write_marker_from_env(name: &str, contents: &str, append: bool) -> Result<(), String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(());
    };
    let path = PathBuf::from(value);
    require_absolute(&path)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut marker = options
        .open(path)
        .map_err(|_| "failed to open marker".to_owned())?;
    marker
        .write_all(contents.as_bytes())
        .map_err(|_| "failed to write marker".to_owned())
}

fn spawn_stderr_descendant() -> Result<(), String> {
    let Some(value) = std::env::var_os(DESCENDANT_PID_MARKER_ENV) else {
        return Ok(());
    };
    let marker = PathBuf::from(value);
    require_absolute(&marker)?;
    let descendant = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| "failed to spawn stderr descendant".to_owned())?;
    std::fs::write(marker, descendant.id().to_string())
        .map_err(|_| "failed to write descendant PID".to_owned())
}

fn require_absolute(path: &Path) -> Result<(), String> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err("marker path was not absolute".to_owned())
    }
}

enum FakeEvent {
    Control(ControlFrame),
    Audio { session: u64, generation: u64 },
    Eof,
    ReadFailed,
}

enum ControlFrame {
    StartSession { session: u64 },
    StartCapture { session: u64 },
    FlushGeneration { session: u64, generation: u64 },
    Shutdown { session: u64 },
    Unexpected,
}
