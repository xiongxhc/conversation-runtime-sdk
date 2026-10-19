use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{timeout_at, Instant};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig, Message,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{invalid_config, transport_failed, OpenAiSpeechRecognitionConfig};
use crate::credential::Credential;
use crate::{AdapterError, AudioFrame};

pub(super) const MAX_WIRE_BYTES: usize = 128 * 1024;
const MAX_QUEUED_FRAMES: usize = 16;
const MAX_QUEUED_PCM_BYTES: usize = 1024 * 1024;
pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Writer = SplitSink<Socket, Message>;
type SendResult = (Writer, Result<Option<AudioFrame>, AdapterError>);
type Sending = Pin<Box<dyn Future<Output = SendResult> + Send>>;

/// Connect, created, update and verification share the caller's ONE absolute
/// startup deadline. No spawned tasks or redirect machinery exist in this path.
pub(super) async fn connect(
    config: &OpenAiSpeechRecognitionConfig,
    credential: &Credential,
) -> Result<Socket, AdapterError> {
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| invalid_config("endpoint cannot form a WebSocket request"))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
        .map_err(|_| invalid_config("credential cannot form an authorization header"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("authorization", authorization);
    let limits = WebSocketConfig::default()
        .read_buffer_size(MAX_WIRE_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WIRE_BYTES * 2)
        .max_message_size(Some(MAX_WIRE_BYTES))
        .max_frame_size(Some(MAX_WIRE_BYTES));
    let (mut socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(limits), true)
        .await
        .map_err(|_| transport_failed())?;
    let created = next_json(&mut socket).await?;
    if created["type"] != "session.created" || created["session"]["type"] != "transcription" {
        return Err(protocol_error("expected transcription session.created"));
    }
    let mut transcription = json!({"model": config.model});
    if let Some(language) = &config.language {
        transcription["language"] = json!(language);
    }
    let session = json!({
        "type": "transcription",
        "audio": {"input": {
            "format": {"type":"audio/pcm","rate":24000},
            "transcription": transcription,
            "turn_detection": {
                "type":"server_vad", "threshold":config.server_vad_threshold,
                "prefix_padding_ms":300, "silence_duration_ms":500
            }
        }}
    });
    socket
        .send(Message::Text(
            json!({"type":"session.update","session":session})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|_| transport_failed())?;
    let updated = next_json(&mut socket).await?;
    let input = &updated["session"]["audio"]["input"];
    let expected = &session["audio"]["input"];
    if updated["type"] != "session.updated"
        || updated["session"]["type"] != "transcription"
        || input["format"] != expected["format"]
        || input["transcription"]["model"] != expected["transcription"]["model"]
        || input["transcription"]["language"] != expected["transcription"]["language"]
        || input["turn_detection"] != expected["turn_detection"]
    {
        return Err(protocol_error(
            "session.updated did not confirm configured transcription",
        ));
    }
    Ok(socket)
}

async fn next_json(socket: &mut Socket) -> Result<Value, AdapterError> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return parse_json(&text),
            Some(Ok(Message::Ping(_))) => socket.flush().await.map_err(|_| transport_failed())?,
            Some(Ok(Message::Pong(_))) => {}
            _ => return Err(protocol_error("incomplete transcription startup")),
        }
    }
}

pub(super) fn parse_json(text: &str) -> Result<Value, AdapterError> {
    if text.len() > MAX_WIRE_BYTES {
        return Err(protocol_error("event exceeded bounded profile"));
    }
    let value: Value = serde_json::from_str(text).map_err(|_| protocol_error("malformed event"))?;
    if !value.is_object() || value["type"].as_str().is_none() {
        return Err(protocol_error("event type is missing"));
    }
    Ok(value)
}

/// A single persistent send future owns the writer until its flush completes.
/// Polling other actor branches cannot cancel/restart it and duplicate PCM.
/// No writer task is spawned: dropping this and the read half closes the socket.
pub(super) struct Outbound {
    writer: Option<Writer>,
    sending: Option<Sending>,
    frames: VecDeque<AudioFrame>,
    frames_including_inflight: usize,
    bytes_including_inflight: usize,
    flush_requested: bool,
    send_budget: Duration,
}

impl Outbound {
    pub(super) fn new(writer: Writer, send_budget: Duration) -> Self {
        Self {
            writer: Some(writer),
            sending: None,
            frames: VecDeque::new(),
            frames_including_inflight: 0,
            bytes_including_inflight: 0,
            flush_requested: false,
            send_budget,
        }
    }

    pub(super) fn enqueue(&mut self, frame: AudioFrame) -> Result<(), AdapterError> {
        let bytes = self
            .bytes_including_inflight
            .checked_add(frame.bytes().len())
            .filter(|bytes| *bytes <= MAX_QUEUED_PCM_BYTES)
            .ok_or_else(|| protocol_error("outbound PCM byte limit exceeded"))?;
        if self.frames_including_inflight >= MAX_QUEUED_FRAMES {
            return Err(protocol_error("outbound PCM frame limit exceeded"));
        }
        self.bytes_including_inflight = bytes;
        self.frames_including_inflight += 1;
        self.frames.push_back(frame);
        Ok(())
    }

    /// Room for one more frame of any admissible size (at most `MAX_PCM_FRAME_BYTES`).
    pub(super) fn has_room(&self) -> bool {
        self.frames_including_inflight < MAX_QUEUED_FRAMES
            && self.bytes_including_inflight + crate::MAX_PCM_FRAME_BYTES <= MAX_QUEUED_PCM_BYTES
    }

    pub(super) fn request_flush(&mut self) {
        self.flush_requested = true;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames_including_inflight == 0
    }

    pub(super) async fn sent(&mut self) -> Result<Option<AudioFrame>, AdapterError> {
        if self.sending.is_none() {
            let frame = self.frames.pop_front();
            if frame.is_none() && !self.flush_requested {
                return std::future::pending().await;
            }
            self.flush_requested = false;
            let mut writer = self.writer.take().expect("idle writer is retained");
            let deadline = Instant::now() + self.send_budget;
            self.sending = Some(Box::pin(async move {
                let result = timeout_at(deadline, async {
                    if let Some(frame) = &frame {
                        let message = json!({"type":"input_audio_buffer.append", "audio":crate::base64::encode(frame.bytes())});
                        writer.send(Message::Text(message.to_string().into())).await
                    } else {
                        writer.flush().await
                    }
                }).await;
                let result = match result {
                    Ok(Ok(())) => Ok(frame),
                    Ok(Err(_)) => Err(transport_failed()),
                    Err(_) => Err(protocol_error("outbound send deadline expired")),
                };
                (writer, result)
            }));
        }
        let (writer, result) = self.sending.as_mut().expect("send started").await;
        self.sending = None;
        self.writer = Some(writer);
        if let Ok(Some(frame)) = &result {
            self.bytes_including_inflight -= frame.bytes().len();
            self.frames_including_inflight -= 1;
        }
        result
    }
}

pub(super) fn protocol_error(detail: &'static str) -> AdapterError {
    AdapterError::new(format!("online speech recognition incomplete: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PcmFormat, PcmSampleFormat};
    use conversation_protocol::{GenerationId, TurnId, UtteranceId};

    // Catches an off-by-one queue admission or premature capacity release: all
    // sixteen 64-KiB PCM frames fit, the seventeenth cannot enter, then each real
    // socket flush releases exactly one frame of credit without duplicated PCM.
    #[tokio::test]
    async fn outbound_bounds_pcm_and_releases_credit_only_after_exact_socket_send() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let client = WebSocketStream::from_raw_socket(
            MaybeTlsStream::Plain(client.unwrap()),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server = WebSocketStream::from_raw_socket(
            accepted.unwrap().0,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let (writer, _reader) = client.split();
        let mut outbound = Outbound::new(writer, Duration::from_secs(5));
        let frame = AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            1,
            PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 64 * 1024],
        )
        .unwrap();
        for _ in 0..16 {
            outbound.enqueue(frame.clone()).unwrap();
        }
        assert!(outbound
            .enqueue(frame.clone())
            .unwrap_err()
            .message()
            .contains("PCM byte limit"));
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut count = 0;
            for _ in 0..16 {
                let (sent, received) = tokio::join!(outbound.sent(), server.next());
                assert_eq!(sent.unwrap().unwrap(), frame);
                let received = received.unwrap().unwrap();
                let value: Value = serde_json::from_str(received.to_text().unwrap()).unwrap();
                assert_eq!(value["type"], "input_audio_buffer.append");
                // 65,536 zero bytes: 21,845 AAAA groups plus AA==.
                assert_eq!(value["audio"], format!("{}AA==", "AAAA".repeat(21_845)));
                count += 1;
            }
            count
        })
        .await
        .unwrap();
        assert_eq!(observed, 16);
        assert!(outbound.is_empty());
        outbound.enqueue(frame).unwrap();
        let tiny = AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            2,
            PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0, 0],
        )
        .unwrap();
        for _ in 0..15 {
            outbound.enqueue(tiny.clone()).unwrap();
        }
        assert!(outbound
            .enqueue(tiny)
            .unwrap_err()
            .message()
            .contains("PCM frame limit"));
    }
}
