//! Authenticated, loopback-testable bridge media listener.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use conversation_model_adapters::{
    frame_for_network_capture, AdapterError, NetworkBridgeRequest, NetworkMediaHub,
    NetworkPlaybackAck, NetworkPlaybackRequest, PcmFormat, PcmSampleFormat, MAX_PCM_FRAME_BYTES,
};
use conversation_protocol::{GenerationId, SessionId};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

const MEDIA_PROTOCOL_VERSION: u8 = 1;
const AUTH_KIND: u8 = 1;
const FRAME_KIND: u8 = 2;
const PLAYBACK_ACK_KIND: u8 = 3;
const FLUSH_REQUEST_KIND: u8 = 4;
const FLUSH_ACK_KIND: u8 = 5;
const CAPTURE_DIRECTION: u8 = 1;
const PLAYBACK_DIRECTION: u8 = 2;
const TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 128;
const FRAME_HEADER_BYTES: usize = 42;
const PLAYBACK_FRAME_HEADER_BYTES: usize = 66;

pub const MAX_MEDIA_ENDPOINT_BYTES: usize = 256;
pub const MAX_MEDIA_PATH_BYTES: usize = 1_024;
pub const MAX_MEDIA_LIFETIME_MS: u64 = 300_000;
pub const MAX_MEDIA_RECONNECT_MS: u64 = 60_000;
pub const MAX_MEDIA_FRAME_RATE: u32 = 1_000;
pub const MAX_MEDIA_QUEUE_BYTES: usize = 1_048_576;

#[derive(Clone, Debug)]
pub struct MediaTlsConfig {
    certificate_path: PathBuf,
    private_key_path: PathBuf,
}

impl MediaTlsConfig {
    pub fn new(certificate_path: PathBuf, private_key_path: PathBuf) -> Result<Self, AdapterError> {
        validate_tls_path(&certificate_path)?;
        validate_tls_path(&private_key_path)?;
        Ok(Self {
            certificate_path,
            private_key_path,
        })
    }
}

#[derive(Clone, Debug)]
pub struct MediaServerConfig {
    bind: SocketAddr,
    advertised_endpoint: String,
    credential_lifetime: Duration,
    reconnect_window: Duration,
    max_frames_per_second: u32,
    max_payload_bytes: usize,
    max_playback_queue_bytes: usize,
    max_playback_queue_duration: Duration,
    tls: Option<MediaTlsConfig>,
}

impl MediaServerConfig {
    pub fn loopback() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            advertised_endpoint: "tcp://127.0.0.1:0".to_owned(),
            credential_lifetime: Duration::from_secs(60),
            reconnect_window: Duration::from_secs(10),
            max_frames_per_second: 100,
            max_payload_bytes: MAX_PCM_FRAME_BYTES,
            max_playback_queue_bytes: 16 * MAX_PCM_FRAME_BYTES,
            max_playback_queue_duration: Duration::from_secs(10),
            tls: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bind: SocketAddr,
        advertised_endpoint: String,
        credential_lifetime: Duration,
        reconnect_window: Duration,
        max_frames_per_second: u32,
        max_payload_bytes: usize,
        max_playback_queue_bytes: usize,
        max_playback_queue_duration: Duration,
        tls: Option<MediaTlsConfig>,
    ) -> Result<Self, AdapterError> {
        let config = Self {
            bind,
            advertised_endpoint,
            credential_lifetime,
            reconnect_window,
            max_frames_per_second,
            max_payload_bytes,
            max_playback_queue_bytes,
            max_playback_queue_duration,
            tls,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    pub fn advertised_endpoint(&self) -> &str {
        &self.advertised_endpoint
    }

    pub fn credential_lifetime(&self) -> Duration {
        self.credential_lifetime
    }

    pub fn reconnect_window(&self) -> Duration {
        self.reconnect_window
    }

    pub fn with_max_frames_per_second(mut self, value: u32) -> Self {
        self.max_frames_per_second = value;
        self
    }

    pub fn with_lifetimes(
        mut self,
        credential_lifetime: Duration,
        reconnect_window: Duration,
    ) -> Self {
        self.credential_lifetime = credential_lifetime;
        self.reconnect_window = reconnect_window;
        self.max_playback_queue_duration = self.max_playback_queue_duration.min(reconnect_window);
        self
    }

    pub fn with_max_playback_queue_bytes(mut self, value: usize) -> Self {
        self.max_playback_queue_bytes = value;
        self
    }

    fn validate(&self) -> Result<(), AdapterError> {
        if !is_private_address(self.bind.ip()) {
            return Err(AdapterError::new(
                "media bind address must be private or loopback",
            ));
        }
        if self.credential_lifetime.is_zero()
            || self.credential_lifetime > Duration::from_millis(MAX_MEDIA_LIFETIME_MS)
            || self.reconnect_window.is_zero()
            || self.reconnect_window > Duration::from_millis(MAX_MEDIA_RECONNECT_MS)
            || self.max_frames_per_second == 0
            || self.max_frames_per_second > MAX_MEDIA_FRAME_RATE
            || self.max_payload_bytes == 0
            || self.max_payload_bytes > MAX_PCM_FRAME_BYTES
            || self.max_playback_queue_bytes == 0
            || self.max_playback_queue_bytes > MAX_MEDIA_QUEUE_BYTES
            || self.max_playback_queue_duration.is_zero()
            || self.max_playback_queue_duration > self.reconnect_window
        {
            return Err(AdapterError::new(
                "media configuration was outside its bounded limits",
            ));
        }
        let (scheme, address) = self
            .advertised_endpoint
            .split_once("://")
            .ok_or_else(|| AdapterError::new("media advertised endpoint was malformed"))?;
        let address = address
            .parse::<SocketAddr>()
            .map_err(|_| AdapterError::new("media advertised endpoint was malformed"))?;
        if self.advertised_endpoint.len() > MAX_MEDIA_ENDPOINT_BYTES
            || !is_private_address(address.ip())
            || (self.bind.port() != 0 && address.port() != self.bind.port())
        {
            return Err(AdapterError::new(
                "media advertised endpoint was outside its bound",
            ));
        }
        let requires_tls = !self.bind.ip().is_loopback();
        if (requires_tls && self.tls.is_none())
            || (!requires_tls && self.tls.is_none() && scheme != "tcp")
            || (self.tls.is_some() && scheme != "tls")
        {
            return Err(AdapterError::new(
                "media transport security did not match its bind address",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct MediaLease {
    session_id: SessionId,
    endpoint: SocketAddr,
    credential: String,
    expires_at: Instant,
    expires_at_ms: u64,
    reconnect_window: Duration,
}

impl MediaLease {
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn credential(&self) -> &str {
        &self.credential
    }

    pub fn reconnect_window(&self) -> Duration {
        self.reconnect_window
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }

    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

pub struct MediaServer {
    inner: Arc<MediaServerInner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

struct MediaServerInner {
    hub: NetworkMediaHub,
    endpoint: SocketAddr,
    advertised_endpoint: String,
    credential_lifetime: Duration,
    reconnect_window: Duration,
    max_frames_per_second: u32,
    max_payload_bytes: usize,
    max_playback_queue_duration: Duration,
    sessions: Mutex<HashMap<u64, MediaSession>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
    cancellation: CancellationToken,
}

struct MediaSession {
    credential: String,
    expires_at: Instant,
    reconnect_deadline: Instant,
    active: bool,
    active_connection_id: Option<u64>,
    next_connection_id: u64,
    expected_sequence: Option<u64>,
    last_capture_timestamp_ms: Option<u64>,
    rate_window_started: Instant,
    frames_in_window: u32,
}

struct MediaDisconnect {
    capture_at_ms: Option<u64>,
    reconnect_deadline: Instant,
}

impl MediaServer {
    pub async fn bind(config: MediaServerConfig) -> Result<Self, AdapterError> {
        config.validate()?;
        let tls = config.tls.as_ref().map(load_tls_acceptor).transpose()?;
        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(|_| AdapterError::new("media listener could not bind"))?;
        let endpoint = listener
            .local_addr()
            .map_err(|_| AdapterError::new("media listener has no local address"))?;
        let inner = Arc::new(MediaServerInner {
            hub: NetworkMediaHub::with_max_playback_queue_bytes(config.max_playback_queue_bytes),
            endpoint,
            advertised_endpoint: bound_advertised_endpoint(
                &config.advertised_endpoint,
                endpoint.port(),
            )?,
            credential_lifetime: config.credential_lifetime,
            reconnect_window: config.reconnect_window,
            max_frames_per_second: config.max_frames_per_second,
            max_payload_bytes: config.max_payload_bytes,
            max_playback_queue_duration: config.max_playback_queue_duration,
            sessions: Mutex::new(HashMap::new()),
            connections: Mutex::new(Vec::new()),
            cancellation: CancellationToken::new(),
        });
        let worker = Arc::clone(&inner);
        let task = tokio::spawn(async move { accept_loop(listener, tls, worker).await });
        Ok(Self {
            inner,
            task: Mutex::new(Some(task)),
        })
    }

    pub fn hub(&self) -> NetworkMediaHub {
        self.inner.hub.clone()
    }

    /// The configured endpoint after replacing a deterministic port-zero bind
    /// with the listener's allocated port. B3 uses this control-plane value;
    /// it is never inferred from an untrusted bridge connection.
    pub fn advertised_endpoint(&self) -> &str {
        &self.inner.advertised_endpoint
    }

    pub async fn reserve(&self, session_id: SessionId) -> Result<MediaLease, AdapterError> {
        self.inner.hub.reserve(session_id).await?;
        let credential = random_credential()?;
        let now = Instant::now();
        let expires_at = now + self.inner.credential_lifetime;
        let expires_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|now| now.checked_add(self.inner.credential_lifetime))
            .and_then(|expires_at| u64::try_from(expires_at.as_millis()).ok())
            .ok_or_else(|| AdapterError::new("media credential expiry could not be calculated"))?;
        let reconnect_window = self.inner.reconnect_window;
        let previous = self.inner.sessions.lock().await.insert(
            session_id.get(),
            MediaSession {
                credential: credential.clone(),
                expires_at,
                reconnect_deadline: now + reconnect_window,
                active: false,
                active_connection_id: None,
                next_connection_id: 1,
                expected_sequence: None,
                last_capture_timestamp_ms: None,
                rate_window_started: now,
                frames_in_window: 0,
            },
        );
        if previous.is_some() {
            self.inner.hub.revoke(session_id).await;
            return Err(AdapterError::new(
                "network media session is already reserved",
            ));
        }
        Ok(MediaLease {
            session_id,
            endpoint: self.inner.endpoint,
            credential,
            expires_at,
            expires_at_ms,
            reconnect_window,
        })
    }

    pub async fn revoke(&self, session_id: SessionId) {
        self.inner.sessions.lock().await.remove(&session_id.get());
        self.inner.hub.revoke(session_id).await;
    }

    pub async fn shutdown(&self) {
        self.inner.cancellation.cancel();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
        let connections = std::mem::take(&mut *self.inner.connections.lock().await);
        for connection in connections {
            let _ = connection.await;
        }
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.inner.cancellation.cancel();
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    inner: Arc<MediaServerInner>,
) {
    loop {
        tokio::select! {
            _ = inner.cancellation.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let worker = Arc::clone(&inner);
                    let tls = tls.clone();
                    let task = tokio::spawn(async move {
                        if let Some(acceptor) = tls {
                            if let Ok(stream) = acceptor.accept(stream).await {
                                let _ = serve_connection(stream, worker).await;
                            }
                        } else {
                            let _ = serve_connection(stream, worker).await;
                        }
                    });
                    let mut connections = inner.connections.lock().await;
                    connections.retain(|connection| !connection.is_finished());
                    connections.push(task);
                }
                Err(_) => return,
            }
        }
    }
}

async fn serve_connection<S>(
    mut stream: S,
    inner: Arc<MediaServerInner>,
) -> Result<(), AdapterError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (session_id, credential) = tokio::select! {
        _ = inner.cancellation.cancelled() => return Ok(()),
        authentication = read_authentication(&mut stream) => authentication?,
    };
    let connection_id = inner.authenticate(session_id, &credential).await?;
    let mut playback = inner
        .hub
        .attach_playback_transport(session_id, connection_id)
        .await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = async {
        loop {
            tokio::select! {
                _ = inner.cancellation.cancelled() => break Ok(()),
                message = read_media_message(&mut reader, inner.max_payload_bytes) => {
                    match message? {
                        MediaMessage::Frame { direction, session_id: frame_session_id, sequence, captured_at_ms, format, payload } => {
                            if frame_session_id != session_id {
                                return Err(AdapterError::new("media frame session identity did not match"));
                            }
                            if direction != CAPTURE_DIRECTION {
                                return Err(AdapterError::new("media bridge direction is not capture"));
                            }
                            let gap = inner
                                .validate_capture_metadata(session_id, sequence, captured_at_ms)
                                .await?;
                            if let Some(at_ms) = gap {
                                inner.hub.deliver_discontinuity(session_id, at_ms).await?;
                            }
                            let frame = frame_for_network_capture(sequence, format, payload)?;
                            inner.hub.deliver_capture(session_id, frame).await?;
                        }
                        MediaMessage::PlaybackAck { session_id: acknowledgement_session_id, sequence, acknowledgement } => {
                            if acknowledgement_session_id != session_id {
                                continue;
                            }
                            let _ = inner.hub.acknowledge_playback(session_id, connection_id, sequence, acknowledgement).await;
                        }
                        MediaMessage::FlushAck { session_id: acknowledgement_session_id, generation_id } => {
                            if acknowledgement_session_id != session_id {
                                continue;
                            }
                            let _ = inner.hub.acknowledge_flush(session_id, connection_id, generation_id).await;
                        }
                    }
                }
                request = playback.recv() => {
                    let request = request.ok_or_else(|| AdapterError::new("network media playback transport closed"))?;
                    match request {
                        NetworkBridgeRequest::Playback(request) => {
                            let sequence = request.sequence();
                            write_playback_frame(&mut writer, session_id, request).await?;
                            inner.hub.mark_playback_written(session_id, connection_id, sequence).await?;
                        }
                        NetworkBridgeRequest::Flush(request) => {
                            write_flush_request(&mut writer, session_id, request.generation_id()).await?;
                        }
                    }
                }
            }
        }
    }
    .await;
    let disconnect = inner.disconnect(session_id, connection_id).await;
    if let Some(disconnect) = disconnect.as_ref() {
        let _ = inner
            .hub
            .disconnect_playback_transport(session_id, connection_id, disconnect.reconnect_deadline)
            .await;
    }
    if let Some(Some(at_ms)) = disconnect.map(|disconnect| disconnect.capture_at_ms) {
        let _ = inner.hub.deliver_discontinuity(session_id, at_ms).await;
    }
    result
}

impl MediaServerInner {
    async fn authenticate(
        &self,
        session_id: SessionId,
        credential: &str,
    ) -> Result<u64, AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("media credential was not accepted"))?;
        let now = Instant::now();
        if credential != session.credential
            || now > session.expires_at
            || now > session.reconnect_deadline
            || session.active
        {
            return Err(AdapterError::new("media credential was not accepted"));
        }
        let connection_id = session.next_connection_id;
        session.next_connection_id = connection_id
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("media connection identity overflowed"))?;
        session.active = true;
        session.active_connection_id = Some(connection_id);
        Ok(connection_id)
    }

    async fn validate_capture_metadata(
        &self,
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
    ) -> Result<Option<u64>, AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("media session was revoked"))?;
        let gap = match session.expected_sequence {
            Some(expected) if sequence < expected => {
                return Err(AdapterError::new("media sequence was replayed"))
            }
            Some(expected) if sequence > expected => Some(expected),
            _ => None,
        };
        let now = Instant::now();
        if now.duration_since(session.rate_window_started) >= Duration::from_secs(1) {
            session.rate_window_started = now;
            session.frames_in_window = 0;
        }
        if session.frames_in_window >= self.max_frames_per_second {
            return Err(AdapterError::new(
                "media frame rate exceeded its configured bound",
            ));
        }
        if session
            .last_capture_timestamp_ms
            .is_some_and(|previous| captured_at_ms <= previous)
        {
            return Err(AdapterError::new(
                "media capture timestamp was not monotonic",
            ));
        }
        session.frames_in_window += 1;
        session.expected_sequence = sequence.checked_add(1);
        session.last_capture_timestamp_ms = Some(captured_at_ms);
        Ok(gap)
    }

    async fn disconnect(
        &self,
        session_id: SessionId,
        connection_id: u64,
    ) -> Option<MediaDisconnect> {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            if session.active_connection_id != Some(connection_id) {
                return None;
            }
            session.active = false;
            session.active_connection_id = None;
            let reconnect_deadline =
                Instant::now() + self.reconnect_window.min(self.max_playback_queue_duration);
            session.reconnect_deadline = reconnect_deadline;
            return Some(MediaDisconnect {
                capture_at_ms: session.last_capture_timestamp_ms,
                reconnect_deadline,
            });
        }
        None
    }
}

async fn read_authentication<R>(stream: &mut R) -> Result<(SessionId, String), AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 6];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
    if prefix != [b'M', b'E', b'D', b'1', MEDIA_PROTOCOL_VERSION, AUTH_KIND] {
        return Err(AdapterError::new(
            "media bridge authentication was malformed",
        ));
    }
    let mut session = [0_u8; 8];
    stream
        .read_exact(&mut session)
        .await
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
    let session = SessionId::new(u64::from_be_bytes(session));
    let length = stream
        .read_u8()
        .await
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?
        as usize;
    if length == 0 || length > MAX_TOKEN_BYTES {
        return Err(AdapterError::new(
            "media bridge authentication was malformed",
        ));
    }
    let mut credential = vec![0_u8; length];
    stream
        .read_exact(&mut credential)
        .await
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
    let credential = String::from_utf8(credential)
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
    Ok((session, credential))
}

enum MediaMessage {
    Frame {
        direction: u8,
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
        format: PcmFormat,
        payload: Vec<u8>,
    },
    PlaybackAck {
        session_id: SessionId,
        sequence: u64,
        acknowledgement: NetworkPlaybackAck,
    },
    FlushAck {
        session_id: SessionId,
        generation_id: GenerationId,
    },
}

async fn read_media_message<R>(
    stream: &mut R,
    max_payload_bytes: usize,
) -> Result<MediaMessage, AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    stream
        .read_exact(&mut header[..6])
        .await
        .map_err(|_| AdapterError::new("media frame was malformed"))?;
    if header[..5] != [b'M', b'E', b'D', b'1', MEDIA_PROTOCOL_VERSION] {
        return Err(AdapterError::new("media frame was malformed"));
    }
    match header[5] {
        FRAME_KIND => read_frame_body(stream, header, max_payload_bytes).await,
        PLAYBACK_ACK_KIND => read_playback_ack(stream).await,
        FLUSH_ACK_KIND => read_flush_ack(stream).await,
        _ => Err(AdapterError::new("media frame was malformed")),
    }
}

async fn read_frame_body<R>(
    stream: &mut R,
    mut header: [u8; FRAME_HEADER_BYTES],
    max_payload_bytes: usize,
) -> Result<MediaMessage, AdapterError>
where
    R: AsyncRead + Unpin,
{
    stream
        .read_exact(&mut header[6..])
        .await
        .map_err(|_| AdapterError::new("media frame was malformed"))?;
    let direction = header[6];
    if direction != CAPTURE_DIRECTION && direction != PLAYBACK_DIRECTION {
        return Err(AdapterError::new("media frame direction was malformed"));
    }
    let session_id = SessionId::new(u64::from_be_bytes(header[7..15].try_into().unwrap()));
    let sequence = u64::from_be_bytes(header[15..23].try_into().unwrap());
    let captured_at_ms = u64::from_be_bytes(header[23..31].try_into().unwrap());
    let sample_format = match header[31] {
        1 => PcmSampleFormat::Signed16LittleEndian,
        2 => PcmSampleFormat::Float32LittleEndian,
        _ => return Err(AdapterError::new("media frame codec was malformed")),
    };
    let sample_rate_hz = u32::from_be_bytes(header[32..36].try_into().unwrap());
    let channels = u16::from_be_bytes(header[36..38].try_into().unwrap());
    let payload_length = u32::from_be_bytes(header[38..42].try_into().unwrap()) as usize;
    if payload_length == 0 || payload_length > max_payload_bytes {
        return Err(AdapterError::new(
            "media frame payload was outside its byte bound",
        ));
    }
    let format = PcmFormat::new(sample_rate_hz, channels, sample_format)?;
    let mut payload = vec![0_u8; payload_length];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|_| AdapterError::new("media frame was truncated"))?;
    Ok(MediaMessage::Frame {
        direction,
        session_id,
        sequence,
        captured_at_ms,
        format,
        payload,
    })
}

async fn read_playback_ack<R>(stream: &mut R) -> Result<MediaMessage, AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut body = [0_u8; 17];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| AdapterError::new("media playback acknowledgement was malformed"))?;
    let acknowledgement = match body[16] {
        1 => NetworkPlaybackAck::Accepted,
        2 => NetworkPlaybackAck::Rendered,
        _ => {
            return Err(AdapterError::new(
                "media playback acknowledgement was malformed",
            ))
        }
    };
    Ok(MediaMessage::PlaybackAck {
        session_id: SessionId::new(u64::from_be_bytes(body[..8].try_into().unwrap())),
        sequence: u64::from_be_bytes(body[8..16].try_into().unwrap()),
        acknowledgement,
    })
}

async fn read_flush_ack<R>(stream: &mut R) -> Result<MediaMessage, AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut body = [0_u8; 16];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| AdapterError::new("media flush acknowledgement was malformed"))?;
    Ok(MediaMessage::FlushAck {
        session_id: SessionId::new(u64::from_be_bytes(body[..8].try_into().unwrap())),
        generation_id: GenerationId::new(u64::from_be_bytes(body[8..16].try_into().unwrap())),
    })
}

async fn write_playback_frame<W>(
    stream: &mut W,
    session_id: SessionId,
    request: NetworkPlaybackRequest,
) -> Result<(), AdapterError>
where
    W: AsyncWrite + Unpin,
{
    let frame = request.frame();
    let format = frame.format();
    let mut header = [0_u8; PLAYBACK_FRAME_HEADER_BYTES];
    header[..7].copy_from_slice(b"MED1\x01\x02\x02");
    header[7..15].copy_from_slice(&session_id.get().to_be_bytes());
    header[15..23].copy_from_slice(&request.sequence().to_be_bytes());
    header[23..31].copy_from_slice(&frame.turn_id().get().to_be_bytes());
    header[31..39].copy_from_slice(&frame.generation_id().get().to_be_bytes());
    header[39..47].copy_from_slice(&frame.utterance_id().get().to_be_bytes());
    header[47..55].copy_from_slice(&frame.sequence().to_be_bytes());
    header[55] = match format.sample_format() {
        PcmSampleFormat::Signed16LittleEndian => 1,
        PcmSampleFormat::Float32LittleEndian => 2,
        _ => return Err(AdapterError::new("media playback codec was malformed")),
    };
    header[56..60].copy_from_slice(&format.sample_rate_hz().to_be_bytes());
    header[60..62].copy_from_slice(&format.channels().to_be_bytes());
    header[62..66].copy_from_slice(&(frame.bytes().len() as u32).to_be_bytes());
    stream
        .write_all(&header)
        .await
        .map_err(|_| AdapterError::new("media playback frame could not be written"))?;
    stream
        .write_all(frame.bytes())
        .await
        .map_err(|_| AdapterError::new("media playback frame could not be written"))?;
    stream
        .flush()
        .await
        .map_err(|_| AdapterError::new("media playback frame could not be written"))
}

async fn write_flush_request<W>(
    stream: &mut W,
    session_id: SessionId,
    generation_id: GenerationId,
) -> Result<(), AdapterError>
where
    W: AsyncWrite + Unpin,
{
    stream
        .write_all(&[
            b'M',
            b'E',
            b'D',
            b'1',
            MEDIA_PROTOCOL_VERSION,
            FLUSH_REQUEST_KIND,
        ])
        .await
        .map_err(|_| AdapterError::new("media flush request could not be written"))?;
    stream
        .write_all(&session_id.get().to_be_bytes())
        .await
        .map_err(|_| AdapterError::new("media flush request could not be written"))?;
    stream
        .write_all(&generation_id.get().to_be_bytes())
        .await
        .map_err(|_| AdapterError::new("media flush request could not be written"))?;
    stream
        .flush()
        .await
        .map_err(|_| AdapterError::new("media flush request could not be written"))
}

fn random_credential() -> Result<String, AdapterError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| AdapterError::new("media credential could not be minted"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || (address.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn validate_tls_path(path: &Path) -> Result<(), AdapterError> {
    if !path.is_absolute()
        || path.as_os_str().len() > MAX_MEDIA_PATH_BYTES
        || path.to_str().is_none()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(AdapterError::new("media TLS material path was invalid"));
    }
    Ok(())
}

fn bound_advertised_endpoint(endpoint: &str, bound_port: u16) -> Result<String, AdapterError> {
    let (scheme, address) = endpoint
        .split_once("://")
        .ok_or_else(|| AdapterError::new("media advertised endpoint was malformed"))?;
    let address = address
        .parse::<SocketAddr>()
        .map_err(|_| AdapterError::new("media advertised endpoint was malformed"))?;
    let port = if address.port() == 0 {
        bound_port
    } else {
        address.port()
    };
    Ok(format!(
        "{scheme}://{}",
        SocketAddr::new(address.ip(), port)
    ))
}

fn load_tls_acceptor(config: &MediaTlsConfig) -> Result<TlsAcceptor, AdapterError> {
    let certificate_file = std::fs::File::open(&config.certificate_path)
        .map_err(|_| AdapterError::new("media TLS material could not be loaded"))?;
    let mut certificate_reader = std::io::BufReader::new(certificate_file);
    let certificates = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AdapterError::new("media TLS material could not be loaded"))?;
    if certificates.is_empty() {
        return Err(AdapterError::new("media TLS material could not be loaded"));
    }
    let private_key_file = std::fs::File::open(&config.private_key_path)
        .map_err(|_| AdapterError::new("media TLS material could not be loaded"))?;
    let mut private_key_reader = std::io::BufReader::new(private_key_file);
    let private_key = rustls_pemfile::private_key(&mut private_key_reader)
        .map_err(|_| AdapterError::new("media TLS material could not be loaded"))?
        .ok_or_else(|| AdapterError::new("media TLS material could not be loaded"))?;
    let configuration = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| AdapterError::new("media TLS material could not be loaded"))?;
    Ok(TlsAcceptor::from(Arc::new(configuration)))
}
