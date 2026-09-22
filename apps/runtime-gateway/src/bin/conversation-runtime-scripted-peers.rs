//! Scripted loopback provider peers for the scripted E2E executable. Not a production service.
//!
//! One process serves all four peers an online meeting needs:
//!
//! - a transcription WebSocket peer speaking the accepted OpenAI streaming wire format,
//! - an OpenAI-compatible chat completions peer returning a scripted answer as SSE,
//! - an OpenAI-compatible speech peer returning a short PCM WAV clip,
//! - a multipart diarization peer returning scripted clusters.
//!
//! On start it writes one JSON line to stdout with the endpoints it bound, then serves until it
//! is killed. Every request is appended, redacted, to the log file named by `--log`: paths,
//! models, counts and byte lengths only. No credential, no audio, no transcript text.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// What the transcription peer answers with once it has seen audio.
const SCRIPTED_TRANSCRIPT: &str = "agenda item";
/// What the language peer answers with.
const SCRIPTED_ANSWER: &str = "approved reply.";
/// Samples in the speech peer's clip; 160 frames of 16 kHz mono is 10 ms of audio.
const SPEECH_SAMPLES: usize = 160;
const SPEECH_SAMPLE: i16 = 0x1234;

#[derive(Clone)]
struct PeerLog(Arc<Mutex<std::fs::File>>);

impl PeerLog {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    /// Appends one redacted JSON line. Callers pass counts and lengths, never content.
    async fn record(&self, entry: Value) {
        let mut file = self.0.lock().await;
        let _ = writeln!(file, "{entry}");
        let _ = file.flush();
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let Some(log_path) = parse_arguments(std::env::args_os().skip(1)) else {
        eprintln!("scripted peers arguments are invalid");
        return std::process::ExitCode::FAILURE;
    };
    let log = match PeerLog::open(&log_path) {
        Ok(log) => log,
        Err(_) => {
            eprintln!("scripted peers log could not be opened");
            return std::process::ExitCode::FAILURE;
        }
    };
    let http = TcpListener::bind("127.0.0.1:0").await.expect("http bind");
    let websocket = TcpListener::bind("127.0.0.1:0").await.expect("ws bind");
    let http_address = http.local_addr().expect("http address");
    let websocket_address = websocket.local_addr().expect("ws address");
    println!(
        "{}",
        json!({
            "http": format!("http://{http_address}/v1"),
            "recognition": format!("ws://{websocket_address}/v1/realtime?intent=transcription"),
        })
    );
    let _ = std::io::stdout().flush();
    tokio::join!(
        serve_http(http, log.clone()),
        serve_websocket(websocket, log),
    );
    std::process::ExitCode::SUCCESS
}

fn parse_arguments(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Option<PathBuf> {
    if arguments.next()?.to_str()? != "--log" {
        return None;
    }
    let path = PathBuf::from(arguments.next()?);
    arguments.next().is_none().then_some(path)
}

// --- HTTP peers ------------------------------------------------------------------------------

async fn serve_http(listener: TcpListener, log: PeerLog) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let log = log.clone();
        tokio::spawn(async move {
            let _ = answer_http(stream, log).await;
        });
    }
}

struct HttpRequest {
    target: String,
    authorization: bool,
    content_type: String,
    body: Vec<u8>,
}

async fn answer_http(mut stream: TcpStream, log: PeerLog) -> Result<(), ()> {
    let request = read_http_request(&mut stream).await?;
    let (path, _) = request
        .target
        .split_once('?')
        .unwrap_or((request.target.as_str(), ""));
    let response = match path {
        path if path.ends_with("/chat/completions") => {
            log.record(json!({
                "peer": "language",
                "target": path,
                "authorization_present": request.authorization,
                "model": json_field(&request.body, "model"),
                "stream": json_field(&request.body, "stream"),
                "message_count": json_array_len(&request.body, "messages"),
                "request_bytes": request.body.len(),
            }))
            .await;
            language_sse_response(SCRIPTED_ANSWER)
        }
        path if path.ends_with("/audio/speech") => {
            log.record(json!({
                "peer": "speech",
                "target": path,
                "authorization_present": request.authorization,
                "model": json_field(&request.body, "model"),
                "voice": json_field(&request.body, "voice"),
                "stream": json_field(&request.body, "stream"),
                "request_bytes": request.body.len(),
            }))
            .await;
            binary_response(&pcm16_wav(16_000, 1, SPEECH_SAMPLES, SPEECH_SAMPLE))
        }
        path if path.ends_with("/audio/transcriptions") => {
            log.record(json!({
                "peer": "diarization",
                "target": path,
                "authorization_present": request.authorization,
                "multipart": request.content_type.starts_with("multipart/form-data"),
                "part_names": multipart_part_names(&request.body),
                "request_bytes": request.body.len(),
            }))
            .await;
            json_response(&diarized_answer())
        }
        path => {
            log.record(json!({"peer": "http", "target": path, "outcome": "not_found"}))
                .await;
            not_found_response()
        }
    };
    stream.write_all(&response).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())?;
    Ok(())
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, ()> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let header = |name: &str| {
        headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let content_length = header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(())?;
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(HttpRequest {
        target: headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or(())?
            .to_owned(),
        authorization: header("authorization").is_some(),
        content_type: header("content-type").unwrap_or_default(),
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

/// A field of a JSON request body, as a value the log can carry. Absent when the body is not
/// JSON, so the diarization peer's multipart body never reaches the log.
fn json_field(body: &[u8], name: &str) -> Value {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get(name).cloned())
        .unwrap_or(Value::Null)
}

fn json_array_len(body: &[u8], name: &str) -> Value {
    match json_field(body, name) {
        Value::Array(items) => json!(items.len()),
        _ => Value::Null,
    }
}

/// The `name="…"` of each multipart part, which is structure rather than content.
fn multipart_part_names(body: &[u8]) -> Vec<String> {
    const MARKER: &[u8] = b"name=\"";
    let mut names = Vec::new();
    let mut offset = 0;
    while let Some(position) = body[offset..]
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        let start = offset + position + MARKER.len();
        let Some(end) = body[start..].iter().position(|byte| *byte == b'"') else {
            break;
        };
        if let Ok(name) = std::str::from_utf8(&body[start..start + end]) {
            names.push(name.to_owned());
        }
        offset = start + end;
    }
    names
}

fn language_sse_response(text: &str) -> Vec<u8> {
    let delta = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})
    );
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in [delta.as_bytes(), b"data: [DONE]\n\n"] {
        response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        response.extend_from_slice(chunk);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"0\r\n\r\n");
    response
}

fn binary_response(body: &[u8]) -> Vec<u8> {
    http_response(body, "application/octet-stream")
}

fn json_response(body: &Value) -> Vec<u8> {
    http_response(body.to_string().as_bytes(), "application/json")
}

fn http_response(body: &[u8], content_type: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn not_found_response() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

/// Two scripted speakers over the first second of the window. The adapter discards these
/// labels and mints its own clusters, which is exactly the behaviour under test.
fn diarized_answer() -> Value {
    json!({
        "task": "transcribe",
        "duration": 1.0,
        "text": "scripted window",
        "segments": [
            {"type":"transcript.text.segment","id":"seg-1","start":0.0,"end":0.4,"text":"scripted","speaker":"A"},
            {"type":"transcript.text.segment","id":"seg-2","start":0.4,"end":0.8,"text":"window","speaker":"B"},
        ],
    })
}

fn pcm16_wav(sample_rate_hz: u32, channels: u16, sample_count: usize, sample: i16) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(sample_count * usize::from(channels) * 2);
    for _ in 0..sample_count * usize::from(channels) {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let data_bytes = u32::try_from(pcm.len()).expect("scripted clip is small");
    let block_align = channels * 2;
    let byte_rate = sample_rate_hz * u32::from(block_align);
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

// --- transcription peer ------------------------------------------------------------------------

async fn serve_websocket(listener: TcpListener, log: PeerLog) {
    loop {
        let Ok((stream, address)) = listener.accept().await else {
            return;
        };
        let log = log.clone();
        tokio::spawn(async move {
            let _ = transcribe(stream, address, log).await;
        });
    }
}

/// The accepted OpenAI streaming transcription exchange: announce the session, confirm the
/// client's `session.update` verbatim, then answer the first uploaded audio with a final
/// transcript. Later appends are still read so the socket never back-pressures the adapter.
async fn transcribe(stream: TcpStream, _address: SocketAddr, log: PeerLog) -> Result<(), ()> {
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };
    use tokio_tungstenite::tungstenite::Message;

    struct CaptureTarget<'a>(&'a mut Option<(String, bool)>);
    impl Callback for CaptureTarget<'_> {
        fn on_request(
            self,
            request: &Request,
            response: Response,
        ) -> Result<Response, ErrorResponse> {
            *self.0 = Some((
                request
                    .uri()
                    .path_and_query()
                    .map(|target| target.as_str().to_owned())
                    .unwrap_or_default(),
                request.headers().contains_key("authorization"),
            ));
            Ok(response)
        }
    }

    let mut handshake = None;
    let mut socket = tokio_tungstenite::accept_hdr_async(stream, CaptureTarget(&mut handshake))
        .await
        .map_err(|_| ())?;
    let (target, authorization) = handshake.ok_or(())?;
    socket
        .send(Message::Text(
            json!({"type":"session.created","session":{"type":"transcription"}})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|_| ())?;

    let update: Value = match socket.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).map_err(|_| ())?,
        _ => return Err(()),
    };
    log.record(json!({
        "peer": "recognition",
        "target": target,
        "authorization_present": authorization,
        "event": "session.update",
        "model": update["session"]["audio"]["input"]["transcription"]["model"].clone(),
        "format": update["session"]["audio"]["input"]["format"].clone(),
        "turn_detection": update["session"]["audio"]["input"]["turn_detection"]["type"].clone(),
    }))
    .await;
    socket
        .send(Message::Text(
            json!({"type":"session.updated","session":update["session"].clone()})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|_| ())?;

    let mut appended = 0_u64;
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(&text).map_err(|_| ())?;
                if payload["type"] != "input_audio_buffer.append" {
                    continue;
                }
                appended += 1;
                log.record(json!({
                    "peer": "recognition",
                    "event": "input_audio_buffer.append",
                    "append_index": appended,
                    // The encoded audio itself never reaches the log.
                    "audio_base64_bytes": payload["audio"].as_str().map_or(0, str::len),
                }))
                .await;
                if appended == 1 {
                    for event in final_transcript_events() {
                        socket
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .map_err(|_| ())?;
                    }
                    log.record(json!({
                        "peer": "recognition",
                        "event": "conversation.item.input_audio_transcription.completed",
                        "transcript_bytes": SCRIPTED_TRANSCRIPT.len(),
                    }))
                    .await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn final_transcript_events() -> [Value; 4] {
    [
        json!({"type":"input_audio_buffer.speech_started","item_id":"scripted-A","audio_start_ms":0}),
        json!({"type":"input_audio_buffer.speech_stopped","item_id":"scripted-A","audio_end_ms":1}),
        json!({"type":"input_audio_buffer.committed","item_id":"scripted-A","previous_item_id":null}),
        json!({
            "type":"conversation.item.input_audio_transcription.completed",
            "item_id":"scripted-A",
            "content_index":0,
            "transcript": SCRIPTED_TRANSCRIPT,
        }),
    ]
}
