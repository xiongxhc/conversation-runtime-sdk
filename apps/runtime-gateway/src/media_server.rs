//! Authenticated, loopback-testable bridge media listener.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use conversation_model_adapters::{
    frame_for_network_capture, AdapterError, CaptureClockPair, CaptureDiscontinuityReason,
    CaptureDomainHistory, CaptureEpoch, CaptureFinishBoundary, CaptureMetadata, FrameProvenance,
    NetworkBridgeRequest, NetworkMediaHub, NetworkPlaybackAck, NetworkPlaybackRequest, PcmFormat,
    PcmSampleFormat, TimedCapture, CAPTURE_TIMING_BYTES, MAX_PCM_FRAME_BYTES,
};
use conversation_protocol::{GenerationId, SessionId};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};
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
const MAX_MEDIA_CONNECTIONS: usize = 64;
const MEDIA_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_CONNECTION_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TIMED_METADATA_PER_SECOND: u32 = 128;
const AUTH_ACCEPTED_KIND: u8 = 9;
const CAPTURE_FINISH_KIND: u8 = 10;
const CAPTURE_FINISH_ACK_KIND: u8 = 11;
const CAPTURE_FINISH_BODY_BYTES: usize = 41;
const CAPTURE_FINISH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaVersion {
    Legacy,
    Timed,
    DrainableTimed,
}

impl MediaVersion {
    /// The exact timed envelope prefix (magic and version byte) this lease
    /// speaks. Legacy leases carry only media-v1 records.
    const fn timed_prefix(self) -> Option<&'static [u8; 5]> {
        match self {
            Self::Legacy => None,
            Self::Timed => Some(b"MED2\x02"),
            Self::DrainableTimed => Some(b"MED3\x03"),
        }
    }
}

pub const MAX_MEDIA_ENDPOINT_BYTES: usize = 256;
pub const MAX_MEDIA_PATH_BYTES: usize = 1_024;
pub const MAX_MEDIA_LIFETIME_MS: u64 = 300_000;
pub const MAX_MEDIA_RECONNECT_MS: u64 = 60_000;
pub const MAX_MEDIA_FRAME_RATE: u32 = 1_000;
pub const MAX_MEDIA_QUEUE_BYTES: usize = 1_048_576;
pub const DEFAULT_MEDIA_PACKET_READ_TIMEOUT_MS: u64 = 5_000;
pub const MAX_MEDIA_PACKET_READ_TIMEOUT_MS: u64 = 60_000;

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
    packet_read_timeout: Duration,
    tls: Option<MediaTlsConfig>,
    capture_format: Option<String>,
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
            packet_read_timeout: Duration::from_millis(DEFAULT_MEDIA_PACKET_READ_TIMEOUT_MS),
            tls: None,
            capture_format: None,
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
        packet_read_timeout: Duration,
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
            packet_read_timeout,
            tls,
            capture_format: None,
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

    pub fn packet_read_timeout(&self) -> Duration {
        self.packet_read_timeout
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

    pub fn with_max_playback_queue_duration(mut self, value: Duration) -> Self {
        self.max_playback_queue_duration = value;
        self
    }

    pub fn with_packet_read_timeout(mut self, value: Duration) -> Self {
        self.packet_read_timeout = value;
        self
    }

    pub fn with_max_payload_bytes(mut self, value: usize) -> Self {
        self.max_payload_bytes = value;
        self
    }

    pub fn with_capture_format(mut self, value: Option<String>) -> Result<Self, AdapterError> {
        if let Some(format) = &value {
            conversation_protocol::validate_capture_format(format)
                .map_err(|_| AdapterError::new("media capture format was invalid"))?;
        }
        self.capture_format = value;
        Ok(self)
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
            || self.packet_read_timeout.is_zero()
            || self.packet_read_timeout > Duration::from_millis(MAX_MEDIA_PACKET_READ_TIMEOUT_MS)
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
    packet_read_timeout: Duration,
    sessions: Mutex<HashMap<u64, MediaSession>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
    connection_permits: Arc<Semaphore>,
    cancellation: CancellationToken,
    next_reservation_epoch: AtomicU64,
    capture_format: Option<String>,
}

struct MediaSession {
    version: MediaVersion,
    reservation_epoch: u64,
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
    timed: Option<TimedMediaState>,
    capture_finished: bool,
    revoked: CancellationToken,
}

#[derive(Clone)]
struct TimedMediaState {
    bridge_session_id: String,
    epoch: Option<CaptureEpoch>,
    domains: CaptureDomainHistory,
    sequence: u64,
    output_end: Option<u64>,
    pair: Option<CaptureClockPair>,
    metadata_window_started: Instant,
    metadata_in_window: u32,
}

struct MediaDisconnect {
    capture_at_ms: Option<u64>,
    capture_epoch: Option<u64>,
    reconnect_deadline: Instant,
}

#[derive(Clone, Copy)]
struct MediaConnection {
    reservation_epoch: u64,
    connection_id: u64,
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
            hub: NetworkMediaHub::with_limits(
                config.max_playback_queue_bytes,
                config.max_playback_queue_duration,
            ),
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
            packet_read_timeout: config.packet_read_timeout,
            sessions: Mutex::new(HashMap::new()),
            connections: Mutex::new(Vec::new()),
            connection_permits: Arc::new(Semaphore::new(MAX_MEDIA_CONNECTIONS)),
            cancellation: CancellationToken::new(),
            next_reservation_epoch: AtomicU64::new(1),
            capture_format: config.capture_format,
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
        self.reserve_version(session_id, MediaVersion::Legacy, None)
            .await
    }

    pub fn capture_format(&self) -> Option<&str> {
        self.inner.capture_format.as_deref()
    }

    pub async fn reserve_timed(
        &self,
        session_id: SessionId,
        bridge_session_id: String,
    ) -> Result<MediaLease, AdapterError> {
        conversation_model_adapters::validate_capture_identifier(&bridge_session_id)?;
        if self.capture_format().is_none() {
            return Err(AdapterError::new("timed capture format is not configured"));
        }
        self.reserve_version(session_id, MediaVersion::Timed, Some(bridge_session_id))
            .await
    }

    pub async fn reserve_drainable_timed(
        &self,
        session_id: SessionId,
        bridge_session_id: String,
    ) -> Result<MediaLease, AdapterError> {
        conversation_model_adapters::validate_capture_identifier(&bridge_session_id)?;
        if self.capture_format().is_none() {
            return Err(AdapterError::new("timed capture format is not configured"));
        }
        self.reserve_version(
            session_id,
            MediaVersion::DrainableTimed,
            Some(bridge_session_id),
        )
        .await
    }

    async fn reserve_version(
        &self,
        session_id: SessionId,
        version: MediaVersion,
        bridge_session_id: Option<String>,
    ) -> Result<MediaLease, AdapterError> {
        let reservation_epoch = self
            .inner
            .next_reservation_epoch
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            })
            .map_err(|_| AdapterError::new("media reservation identity overflowed"))?;
        self.inner
            .hub
            .reserve(session_id, reservation_epoch)
            .await?;
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
                version,
                reservation_epoch,
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
                timed: bridge_session_id.map(|bridge_session_id| TimedMediaState {
                    bridge_session_id,
                    epoch: None,
                    domains: CaptureDomainHistory::default(),
                    sequence: 0,
                    output_end: None,
                    pair: None,
                    metadata_window_started: now,
                    metadata_in_window: 0,
                }),
                capture_finished: false,
                revoked: CancellationToken::new(),
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
        if let Some(session) = self.inner.sessions.lock().await.remove(&session_id.get()) {
            session.revoked.cancel();
        }
        self.inner.hub.revoke(session_id).await;
    }

    pub async fn shutdown(&self) {
        self.inner.cancellation.cancel();
        self.inner.hub.cancel_expiry_workers();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
        let connections = std::mem::take(&mut *self.inner.connections.lock().await);
        for mut connection in connections {
            if tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, &mut connection)
                .await
                .is_err()
            {
                connection.abort();
                let _ = connection.await;
            }
        }
        self.inner.hub.shutdown().await;
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.inner.cancellation.cancel();
        self.inner.hub.cancel_expiry_workers();
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
                    let Ok(permit) = Arc::clone(&inner.connection_permits).try_acquire_owned() else {
                        continue;
                    };
                    let task = tokio::spawn(async move {
                        let _permit = permit;
                        if let Some(acceptor) = tls {
                            let handshake = tokio::select! {
                                _ = worker.cancellation.cancelled() => return,
                                result = tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, acceptor.accept(stream)) => result,
                            };
                            if let Ok(Ok(stream)) = handshake {
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

/// One connection's pending explicit capture finish. `operation` is the single
/// pinned future the connection loop owns and polls; dropping this state drops
/// the barrier receiver, so cancellation can never leave an orphaned waiter.
struct PendingFinish {
    boundary: CaptureFinishBoundary,
    deadline: tokio::time::Instant,
    revoked: CancellationToken,
    operation: Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send>>,
}

/// The exact kind 11 was written and flushed and the write side half-closed.
/// Only the producer's clean EOF completes the connection; the same absolute
/// finish deadline still bounds that wait.
struct AckedFinish {
    deadline: tokio::time::Instant,
    revoked: CancellationToken,
}

async fn serve_connection<S>(
    mut stream: S,
    inner: Arc<MediaServerInner>,
) -> Result<(), AdapterError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handshake_deadline = tokio::time::Instant::now() + MEDIA_HANDSHAKE_TIMEOUT;
    let (session_id, credential, bridge_session_id, version) = tokio::select! {
        _ = inner.cancellation.cancelled() => return Ok(()),
        authentication = tokio::time::timeout_at(handshake_deadline, read_authentication(&mut stream)) => {
            authentication
                .map_err(|_| AdapterError::new("media bridge authentication timed out"))??
        },
    };
    let timed = bridge_session_id.is_some();
    let connection = if timed {
        tokio::select! {
            biased;
            _ = inner.cancellation.cancelled() => return Ok(()),
            _ = tokio::time::sleep_until(handshake_deadline) => return Err(AdapterError::new("media bridge authentication timed out")),
            result = inner.authenticate(session_id, &credential, bridge_session_id.as_deref(), version) => result?,
        }
    } else {
        inner
            .authenticate(session_id, &credential, None, version)
            .await?
    };
    let mut epoch_started = false;
    let mut media_phase = false;
    let mut planned = false;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = MediaPacketReader::new(
        reader,
        version,
        inner.max_payload_bytes,
        inner.packet_read_timeout,
    );
    let result = async {
        if let Some(bridge) = bridge_session_id.as_deref() {
            tokio::select! {
                biased;
                _ = inner.cancellation.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(handshake_deadline) => return Err(AdapterError::new("media bridge authentication timed out")),
                result = write_authentication_accepted(&mut writer, session_id, bridge, version) => result?,
            }
        }
        // Every operation after active-state assignment is inside this result
        // scope. ACK/flush, cancellation and attachment failures all reach the
        // same exact-connection cleanup below; no playback precedes kind9.
        let mut playback = tokio::select! {
            biased;
            _ = inner.cancellation.cancelled() => return Ok(()),
            result = inner.hub.attach_playback_transport_for_reservation(session_id, connection.reservation_epoch, connection.connection_id) => result?,
        };
        media_phase = true;
        // Connection state: running while both are `None`; finishPending while
        // `finish` is `Some`; acked while `acked` is `Some`. Both later states
        // disable outbound playback, refuse every later timed record, and are
        // bounded by the one absolute finish deadline; the socket reader stays
        // live so media-v1 acknowledgements remain bounded no-ops instead of
        // stalling the peer, and the producer's clean EOF after the ACK is the
        // planned completion.
        let mut finish: Option<PendingFinish> = None;
        let mut acked: Option<AckedFinish> = None;
        loop {
            let deadline = finish
                .as_ref()
                .map(|pending| pending.deadline)
                .or_else(|| acked.as_ref().map(|acked| acked.deadline));
            let revoked = finish
                .as_ref()
                .map(|pending| pending.revoked.clone())
                .or_else(|| acked.as_ref().map(|acked| acked.revoked.clone()));
            let closing = finish.is_some() || acked.is_some();
            tokio::select! {
                biased;
                _ = inner.cancellation.cancelled() => break Ok(()),
                _ = async { revoked.as_ref().expect("finish pending").cancelled().await }, if revoked.is_some() => {
                    break Err(AdapterError::new("media session was revoked before capture finish"));
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    break Err(AdapterError::new("capture finish timed out"));
                }
                result = async { finish.as_mut().expect("finish pending").operation.as_mut().await }, if finish.is_some() => {
                    result?;
                    let pending = finish.take().expect("finish pending");
                    tokio::time::timeout_at(
                        pending.deadline,
                        write_capture_finish_ack(&mut writer, &pending.boundary),
                    )
                    .await
                    .map_err(|_| AdapterError::new("capture finish acknowledgement timed out"))??;
                    // Half-close so the producer sees FIN right after the ACK
                    // bytes; the read side stays open for its own clean EOF.
                    tokio::time::timeout_at(pending.deadline, writer.shutdown())
                        .await
                        .map_err(|_| AdapterError::new("capture finish acknowledgement timed out"))?
                        .map_err(|_| AdapterError::new("capture finish acknowledgement flush failed"))?;
                    acked = Some(AckedFinish { deadline: pending.deadline, revoked: pending.revoked });
                }
                message = reader.next_message() => {
                    let Some(message) = message? else {
                        if acked.is_some() {
                            planned = true;
                            break Ok(());
                        }
                        return Err(AdapterError::new("media frame was malformed"));
                    };
                    match message {
                        MediaMessage::Timed {session_id:source,record}=>{
                            if !timed||source!=session_id {return Err(AdapterError::new("timed media identity did not match"));}
                            if closing {return Err(AdapterError::new("timed capture is finished"));}
                            if let TimedMediaRecord::Finish(boundary)=record {
                                if !epoch_started {return Err(AdapterError::new("timed media epoch is missing"));}
                                let revoked=inner.admit_capture_finish(session_id,connection,&boundary).await?;
                                finish=Some(PendingFinish {
                                    deadline:tokio::time::Instant::now()+CAPTURE_FINISH_TIMEOUT,
                                    revoked,
                                    operation:Box::pin(finish_operation(Arc::clone(&inner),session_id,connection,boundary.clone())),
                                    boundary,
                                });
                                continue;
                            }
                            inner.admit_timed_rate(session_id,matches!(&record,TimedMediaRecord::Metadata(_))).await?;
                            match record {
                                TimedMediaRecord::Metadata(CaptureMetadata::Epoch(epoch))=>{
                                    inner.admit_epoch(session_id,&epoch,!epoch_started).await?;
                                    inner.hub.deliver_capture_metadata(session_id,CaptureMetadata::Epoch(epoch)).await?;
                                    epoch_started=true;
                                }
                                TimedMediaRecord::Metadata(metadata)=>{
                                    if !epoch_started {return Err(AdapterError::new("timed media epoch is missing"));}
                                    inner.admit_timed_metadata(session_id,&metadata).await?;
                                    if matches!(metadata,CaptureMetadata::Discontinuity {reason:CaptureDiscontinuityReason::SourceDrop|CaptureDiscontinuityReason::SequenceHole|CaptureDiscontinuityReason::Reconnect|CaptureDiscontinuityReason::Pause|CaptureDiscontinuityReason::SourceDiscontinuity,..}) {inner.hub.deliver_discontinuity(session_id,0).await?;}
                                    inner.hub.deliver_capture_metadata(session_id,metadata).await?;
                                }
                                TimedMediaRecord::Frame {format,timing,payload}=>{
                                    if !epoch_started {return Err(AdapterError::new("timed media epoch is missing"));}
                                    let (epoch,hole)=inner.validate_timed_frame(session_id,&timing,format,payload.len()).await?;
                                    if hole {
                                        inner.hub.deliver_discontinuity(session_id,0).await?;
                                        inner.hub.deliver_capture_metadata(session_id,CaptureMetadata::Discontinuity {capture_epoch:epoch.capture_epoch,reason:CaptureDiscontinuityReason::SequenceHole}).await?;
                                    }
                                    let frame=frame_for_network_capture(timing.sequence,format,payload)?.with_capture_timing(TimedCapture {epoch,frame:timing.clone()})?;
                                    inner.hub.deliver_capture(session_id,frame).await?;
                                    let mut sessions=inner.sessions.lock().await;
                                    let state=sessions.get_mut(&session_id.get()).and_then(|session|session.timed.as_mut()).ok_or_else(||AdapterError::new("timed media session was revoked"))?;
                                    state.sequence=timing.sequence;state.output_end=Some(timing.output_start_sample+u64::from(timing.sample_count));
                                }
                                TimedMediaRecord::Finish(_)=>unreachable!("finish admitted above"),
                            }
                        }
                        MediaMessage::Frame { direction, session_id: frame_session_id, sequence, captured_at_ms, format, payload } => {
                            if timed {return Err(AdapterError::new("legacy capture is not permitted for timed media"));}
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
                            inner
                                .commit_capture_metadata(session_id, sequence, captured_at_ms)
                                .await?;
                        }
                        MediaMessage::PlaybackAck { session_id: acknowledgement_session_id, sequence, acknowledgement } => {
                            if acknowledgement_session_id != session_id {
                                continue;
                            }
                            let _ = inner.hub.acknowledge_playback(session_id, connection.connection_id, sequence, acknowledgement).await;
                        }
                        MediaMessage::FlushAck { session_id: acknowledgement_session_id, generation_id } => {
                            if acknowledgement_session_id != session_id {
                                continue;
                            }
                            let _ = inner.hub.acknowledge_flush(session_id, connection.connection_id, generation_id).await;
                        }
                    }
                }
                request = playback.recv(), if !closing => {
                    let request = request.ok_or_else(|| AdapterError::new("network media playback transport closed"))?;
                    match request {
                        NetworkBridgeRequest::Playback(request) => {
                            let sequence = request.sequence();
                            tokio::time::timeout(
                                inner.max_playback_queue_duration,
                                write_playback_frame(&mut writer, session_id, request),
                            )
                            .await
                            .map_err(|_| AdapterError::new("media playback frame write timed out"))??;
                            inner.hub.mark_playback_written(session_id, connection.connection_id, sequence).await?;
                        }
                        NetworkBridgeRequest::Flush(request) => {
                            tokio::time::timeout(
                                inner.max_playback_queue_duration,
                                write_flush_request(&mut writer, session_id, request.generation_id()),
                            )
                            .await
                            .map_err(|_| AdapterError::new("media flush request write timed out"))??;
                        }
                    }
                }
            }
        }
    }
    .await;
    if planned {
        // Planned completion: the transport was already detached by the finish
        // operation and the recorder holds the acknowledged boundary. No
        // reconnect window and no synthetic disconnect capture metadata.
        inner
            .release_planned_connection(session_id, connection)
            .await;
    } else {
        cleanup_connection(&inner, session_id, connection, media_phase).await;
    }
    result
}

/// The whole finish sequence after admission, in order: planned playback
/// detach (which delivers any `Flushed` receipts first), recorder barrier
/// admission under the hub's bounded operation timeout, then the single
/// take-once completion. Success means only that the recorder matched and
/// flushed the boundary.
async fn finish_operation(
    inner: Arc<MediaServerInner>,
    session_id: SessionId,
    connection: MediaConnection,
    boundary: CaptureFinishBoundary,
) -> Result<(), AdapterError> {
    inner
        .hub
        .planned_detach_playback_transport_for_finish(
            session_id,
            connection.reservation_epoch,
            connection.connection_id,
        )
        .await?;
    let completion = inner
        .hub
        .deliver_capture_finish_for_reservation(
            session_id,
            connection.reservation_epoch,
            connection.connection_id,
            boundary,
        )
        .await?;
    completion
        .await
        .map_err(|_| AdapterError::new("capture finish barrier was dropped"))?
}

async fn cleanup_connection(
    inner: &MediaServerInner,
    session_id: SessionId,
    connection: MediaConnection,
    media_phase: bool,
) {
    let Some(disconnect) = inner.disconnect_snapshot(session_id, connection).await else {
        return;
    };
    // Keep this connection's admission fence until old notifications settle.
    // Revoke/re-reserve remains possible; hub operations independently fence the
    // reservation and retain old sinks, never a global lock across a send.
    tokio::select! {
        biased;
        _ = inner.cancellation.cancelled() => {},
        _ = async {
            let _ = inner.hub.disconnect_playback_transport_for_reservation(session_id, connection.reservation_epoch, connection.connection_id, disconnect.reconnect_deadline).await;
            if media_phase {
                if let Some(at_ms) = disconnect.capture_at_ms {
                    let _ = inner.hub.deliver_disconnect_for_reservation(session_id, connection.reservation_epoch, at_ms, disconnect.capture_epoch).await;
                }
            }
        } => {},
    }
    inner
        .release_connection(session_id, connection, disconnect.reconnect_deadline)
        .await;
}

async fn write_authentication_accepted<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session_id: SessionId,
    bridge_session_id: &str,
    version: MediaVersion,
) -> Result<(), AdapterError> {
    conversation_model_adapters::validate_capture_identifier(bridge_session_id)?;
    let prefix = version.timed_prefix().ok_or_else(|| {
        AdapterError::new("media authentication confirmation requires a timed lease")
    })?;
    let mut packet = Vec::with_capacity(83);
    packet.extend_from_slice(prefix);
    packet.push(AUTH_ACCEPTED_KIND);
    packet.extend_from_slice(&(9 + bridge_session_id.len() as u32).to_be_bytes());
    packet.extend_from_slice(&session_id.get().to_be_bytes());
    packet.push(bridge_session_id.len() as u8);
    packet.extend_from_slice(bridge_session_id.as_bytes());
    writer
        .write_all(&packet)
        .await
        .map_err(|_| AdapterError::new("media authentication confirmation write failed"))?;
    writer
        .flush()
        .await
        .map_err(|_| AdapterError::new("media authentication confirmation flush failed"))
}

fn admit_rate_window(
    started: &mut Instant,
    count: &mut u32,
    limit: u32,
    now: Instant,
) -> Result<(), AdapterError> {
    if now.duration_since(*started) >= Duration::from_secs(1) {
        *started = now;
        *count = 0;
    }
    if *count >= limit {
        return Err(AdapterError::new(
            "timed media record rate exceeded its bound",
        ));
    }
    *count += 1;
    Ok(())
}

impl MediaServerInner {
    async fn admit_timed_rate(
        &self,
        session_id: SessionId,
        is_metadata: bool,
    ) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        if session.capture_finished {
            return Err(AdapterError::new("timed capture is finished"));
        }
        let now = Instant::now();
        if is_metadata {
            let state = session
                .timed
                .as_mut()
                .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
            admit_rate_window(
                &mut state.metadata_window_started,
                &mut state.metadata_in_window,
                MAX_TIMED_METADATA_PER_SECOND,
                now,
            )
        } else {
            admit_rate_window(
                &mut session.rate_window_started,
                &mut session.frames_in_window,
                self.max_frames_per_second,
                now,
            )
        }
    }

    async fn admit_epoch(
        &self,
        session_id: SessionId,
        epoch: &CaptureEpoch,
        reconnect: bool,
    ) -> Result<(), AdapterError> {
        epoch.validate()?;
        let mut sessions = self.sessions.lock().await;
        let state = sessions
            .get_mut(&session_id.get())
            .and_then(|session| session.timed.as_mut())
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        if state.bridge_session_id != epoch.bridge_session_id {
            return Err(AdapterError::new("timed bridge identity did not match"));
        }
        if state.epoch.as_ref().is_some_and(|prior| {
            prior.clock_domain_id == epoch.clock_domain_id
                && (epoch.capture_epoch < prior.capture_epoch
                    || (!reconnect && epoch.capture_epoch == prior.capture_epoch))
        }) {
            return Err(AdapterError::new("timed capture epoch was replayed"));
        }
        // Before any watermark mutation: a rejected new/retired domain must not
        // reset sequence state and authorize replay on the next reconnect.
        state.domains.admit(&epoch.clock_domain_id)?;
        if let Some(prior) = &state.epoch {
            if prior.clock_domain_id == epoch.clock_domain_id {
                if epoch.capture_epoch > prior.capture_epoch {
                    state.sequence = 0;
                    state.output_end = None;
                }
            } else {
                state.sequence = 0;
                state.output_end = None;
                state.pair = None;
            }
        }
        state.epoch = Some(epoch.clone());
        Ok(())
    }

    async fn admit_timed_metadata(
        &self,
        session_id: SessionId,
        metadata: &CaptureMetadata,
    ) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let state = sessions
            .get_mut(&session_id.get())
            .and_then(|session| session.timed.as_mut())
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        let epoch = state
            .epoch
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed media epoch is missing"))?;
        match metadata {
            CaptureMetadata::ClockPair(pair) => {
                pair.validate()?;
                if pair.clock_domain_id != epoch.clock_domain_id
                    || state.pair.as_ref().is_some_and(|prior| {
                        pair.sequence <= prior.sequence
                            || pair.midpoint_twice() <= prior.midpoint_twice()
                            || pair.wall_epoch < prior.wall_epoch
                    })
                {
                    return Err(AdapterError::new(
                        "timed clock pair identity or ordering was invalid",
                    ));
                }
                state.pair = Some(pair.clone());
            }
            CaptureMetadata::Discontinuity {
                capture_epoch,
                reason,
            } => {
                if *capture_epoch != epoch.capture_epoch
                    || matches!(reason, CaptureDiscontinuityReason::AlignmentLimitExceeded)
                {
                    return Err(AdapterError::new("timed discontinuity was invalid"));
                }
            }
            CaptureMetadata::Epoch(_) => {
                return Err(AdapterError::new("timed epoch admission was invalid"))
            }
        }
        Ok(())
    }

    async fn validate_timed_frame(
        &self,
        session_id: SessionId,
        timing: &FrameProvenance,
        format: PcmFormat,
        length: usize,
    ) -> Result<(CaptureEpoch, bool), AdapterError> {
        timing.validate_pcm(format, length)?;
        let configured = self
            .capture_format
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed capture format is not configured"))?;
        let mut parts = configured.split('/');
        let codec = parts.next().unwrap();
        let rate = parts.next().unwrap().parse::<u32>().unwrap();
        let channels = parts.next().unwrap().parse::<u16>().unwrap();
        let expected = PcmFormat::new(
            rate,
            channels,
            if codec == "pcm_s16le" {
                PcmSampleFormat::Signed16LittleEndian
            } else {
                PcmSampleFormat::Float32LittleEndian
            },
        )?;
        if format != expected {
            return Err(AdapterError::new(
                "timed capture format did not match lease",
            ));
        }
        let sessions = self.sessions.lock().await;
        let state = sessions
            .get(&session_id.get())
            .and_then(|session| session.timed.as_ref())
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        let epoch = state
            .epoch
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed media epoch is missing"))?;
        if timing.capture_epoch != epoch.capture_epoch
            || timing.sequence <= state.sequence
            || state
                .output_end
                .is_some_and(|end| timing.output_start_sample < end)
        {
            return Err(AdapterError::new(
                "timed capture sequence or epoch was replayed",
            ));
        }
        let hole = state.sequence > 0
            && (state.sequence.checked_add(1) != Some(timing.sequence)
                || state.output_end != Some(timing.output_start_sample));
        Ok((epoch.clone(), hole))
    }

    async fn authenticate(
        &self,
        session_id: SessionId,
        credential: &str,
        bridge_session_id: Option<&str>,
        version: MediaVersion,
    ) -> Result<MediaConnection, AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("media credential was not accepted"))?;
        let now = Instant::now();
        if credential != session.credential
            || session.version != version
            || session
                .timed
                .as_ref()
                .map(|state| state.bridge_session_id.as_str())
                != bridge_session_id
            || now > session.expires_at
            || now > session.reconnect_deadline
            || session.active
            || session.capture_finished
        {
            return Err(AdapterError::new("media credential was not accepted"));
        }
        let connection_id = session.next_connection_id;
        session.next_connection_id = connection_id
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("media connection identity overflowed"))?;
        session.active = true;
        session.active_connection_id = Some(connection_id);
        Ok(MediaConnection {
            reservation_epoch: session.reservation_epoch,
            connection_id,
        })
    }

    async fn validate_capture_metadata(
        &self,
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
    ) -> Result<Option<u64>, AdapterError> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(&session_id.get())
            .ok_or_else(|| AdapterError::new("media session was revoked"))?;
        let gap = match session.expected_sequence {
            Some(expected) if sequence < expected => {
                return Err(AdapterError::new("media sequence was replayed"))
            }
            Some(expected) if sequence > expected => Some(captured_at_ms),
            _ => None,
        };
        sequence
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("media sequence overflowed"))?;
        let now = Instant::now();
        let frames_in_window =
            if now.duration_since(session.rate_window_started) >= Duration::from_secs(1) {
                0
            } else {
                session.frames_in_window
            };
        if frames_in_window >= self.max_frames_per_second {
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
        Ok(gap)
    }

    async fn commit_capture_metadata(
        &self,
        session_id: SessionId,
        sequence: u64,
        captured_at_ms: u64,
    ) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("media session was revoked"))?;
        if session
            .expected_sequence
            .is_some_and(|expected| sequence < expected)
        {
            return Err(AdapterError::new("media sequence was replayed"));
        }
        let next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("media sequence overflowed"))?;
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
        session.expected_sequence = Some(next_sequence);
        session.last_capture_timestamp_ms = Some(captured_at_ms);
        Ok(())
    }

    async fn disconnect_snapshot(
        &self,
        session_id: SessionId,
        connection: MediaConnection,
    ) -> Option<MediaDisconnect> {
        if let Some(session) = self.sessions.lock().await.get(&session_id.get()) {
            if session.reservation_epoch != connection.reservation_epoch
                || session.active_connection_id != Some(connection.connection_id)
            {
                return None;
            }
            let reconnect_deadline =
                Instant::now() + self.reconnect_window.min(self.max_playback_queue_duration);
            return Some(MediaDisconnect {
                capture_at_ms: session.last_capture_timestamp_ms.or_else(|| {
                    session
                        .timed
                        .as_ref()
                        .filter(|state| state.sequence > 0)
                        .map(|_| 0)
                }),
                capture_epoch: session
                    .timed
                    .as_ref()
                    .and_then(|state| state.epoch.as_ref())
                    .map(|epoch| epoch.capture_epoch),
                reconnect_deadline,
            });
        }
        None
    }

    async fn release_connection(
        &self,
        session_id: SessionId,
        connection: MediaConnection,
        reconnect_deadline: Instant,
    ) {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            if session.reservation_epoch == connection.reservation_epoch
                && session.active_connection_id == Some(connection.connection_id)
            {
                session.active = false;
                session.active_connection_id = None;
                session.reconnect_deadline = reconnect_deadline;
            }
        }
    }

    /// Admits exactly one capture finish for the authenticated drainable
    /// connection: the wire boundary must equal the current timed watermarks
    /// (epoch, last frame sequence and output end, final clock pair). On
    /// success the capture lane is marked finished so no later frame,
    /// metadata, finish or reconnect is admitted, and the session's revocation
    /// token is returned so the pending finish stays interruptible.
    async fn admit_capture_finish(
        &self,
        session_id: SessionId,
        connection: MediaConnection,
        boundary: &CaptureFinishBoundary,
    ) -> Result<CancellationToken, AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        if session.reservation_epoch != connection.reservation_epoch
            || session.active_connection_id != Some(connection.connection_id)
        {
            return Err(AdapterError::new("capture finish connection did not match"));
        }
        if session.version != MediaVersion::DrainableTimed {
            return Err(AdapterError::new(
                "capture finish requires a drainable lease",
            ));
        }
        if session.capture_finished {
            return Err(AdapterError::new("capture finish was duplicated"));
        }
        let state = session
            .timed
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed media session was revoked"))?;
        let epoch = state
            .epoch
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed media epoch is missing"))?;
        let pair = state
            .pair
            .as_ref()
            .ok_or_else(|| AdapterError::new("timed clock pair is missing"))?;
        let frame = match (state.sequence, state.output_end) {
            (0, None) => None,
            (sequence, Some(output_end)) if sequence > 0 => Some((sequence, output_end)),
            _ => {
                return Err(AdapterError::new(
                    "timed capture watermark was inconsistent",
                ))
            }
        };
        if boundary.capture_epoch() != epoch.capture_epoch
            || boundary.final_clock_pair_sequence() != pair.sequence
            || boundary.frame() != frame
        {
            return Err(AdapterError::new("capture finish boundary did not match"));
        }
        session.capture_finished = true;
        Ok(session.revoked.clone())
    }

    /// Planned completion after an acknowledged finish: only the active flag
    /// is released. The reservation, watermarks and finished capture lane stay
    /// until control-plane voice stop revokes them.
    async fn release_planned_connection(&self, session_id: SessionId, connection: MediaConnection) {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            if session.reservation_epoch == connection.reservation_epoch
                && session.active_connection_id == Some(connection.connection_id)
            {
                session.active = false;
                session.active_connection_id = None;
            }
        }
    }

    #[cfg(test)]
    async fn disconnect(
        &self,
        session_id: SessionId,
        connection: MediaConnection,
    ) -> Option<MediaDisconnect> {
        let snapshot = self.disconnect_snapshot(session_id, connection).await?;
        self.release_connection(session_id, connection, snapshot.reconnect_deadline)
            .await;
        Some(snapshot)
    }
}

async fn read_authentication<R>(
    stream: &mut R,
) -> Result<(SessionId, String, Option<String>, MediaVersion), AdapterError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 6];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
    let timed_version = match &prefix {
        b"MED2\x02\x01" => Some(MediaVersion::Timed),
        b"MED3\x03\x01" => Some(MediaVersion::DrainableTimed),
        _ => None,
    };
    if let Some(version) = timed_version {
        let length = stream
            .read_u32()
            .await
            .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?
            as usize;
        if !(11..=202).contains(&length) {
            return Err(AdapterError::new(
                "media bridge authentication was malformed",
            ));
        }
        let mut body = vec![0; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?;
        let session = SessionId::new(u64::from_be_bytes(body[..8].try_into().unwrap()));
        let token_len = usize::from(body[8]);
        if token_len == 0 || token_len > MAX_TOKEN_BYTES || 9 + token_len >= body.len() {
            return Err(AdapterError::new(
                "media bridge authentication was malformed",
            ));
        }
        let id_len = usize::from(body[9 + token_len]);
        if body.len() != 10 + token_len + id_len {
            return Err(AdapterError::new(
                "media bridge authentication was malformed",
            ));
        }
        let credential = std::str::from_utf8(&body[9..9 + token_len])
            .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?
            .to_owned();
        let bridge = std::str::from_utf8(&body[10 + token_len..])
            .map_err(|_| AdapterError::new("media bridge authentication was malformed"))?
            .to_owned();
        conversation_model_adapters::validate_capture_identifier(&bridge)?;
        return Ok((session, credential, Some(bridge), version));
    }
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
    Ok((session, credential, None, MediaVersion::Legacy))
}

enum MediaMessage {
    Timed {
        session_id: SessionId,
        record: TimedMediaRecord,
    },
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

enum TimedMediaRecord {
    Frame {
        format: PcmFormat,
        timing: FrameProvenance,
        payload: Vec<u8>,
    },
    Metadata(CaptureMetadata),
    Finish(CaptureFinishBoundary),
}

/// How much of the current packet the reader must hold before it can act.
enum PacketTarget {
    /// More header bytes are needed before the packet length is known.
    Header(usize),
    /// The complete packet occupies exactly this many bytes.
    Packet(usize),
}

const MEDIA_PREFIX_BYTES: usize = 6;
const TIMED_HEADER_BYTES: usize = 10;

/// Inspects the bytes buffered so far and either rejects a malformed prefix,
/// asks for more header bytes, or states the exact packet length. Every
/// bound is checked before the corresponding bytes are requested, so the
/// reader never consumes a payload the envelope already disqualified.
fn packet_target(
    buffer: &[u8],
    version: MediaVersion,
    max_payload_bytes: usize,
) -> Result<PacketTarget, AdapterError> {
    if buffer.len() < MEDIA_PREFIX_BYTES {
        return Ok(PacketTarget::Header(MEDIA_PREFIX_BYTES));
    }
    let kind = buffer[5];
    if version
        .timed_prefix()
        .is_some_and(|prefix| prefix == &buffer[..5])
    {
        if buffer.len() < TIMED_HEADER_BYTES {
            return Ok(PacketTarget::Header(TIMED_HEADER_BYTES));
        }
        let invalid = || AdapterError::new("timed media record was malformed");
        let length = u32::from_be_bytes(buffer[6..10].try_into().unwrap()) as usize;
        if kind == CAPTURE_FINISH_KIND {
            if version != MediaVersion::DrainableTimed || length != CAPTURE_FINISH_BODY_BYTES {
                return Err(AdapterError::new("capture finish record was malformed"));
            }
            return Ok(PacketTarget::Packet(TIMED_HEADER_BYTES + length));
        }
        let maximum = if kind == 2 {
            max_payload_bytes + 107
        } else {
            256
        };
        if length < 8 || length > maximum || ![2, 6, 7, 8].contains(&kind) {
            return Err(invalid());
        }
        return Ok(PacketTarget::Packet(TIMED_HEADER_BYTES + length));
    }
    if buffer[..5] != [b'M', b'E', b'D', b'1', MEDIA_PROTOCOL_VERSION] {
        return Err(AdapterError::new("media frame was malformed"));
    }
    match kind {
        FRAME_KIND => {
            if buffer.len() < FRAME_HEADER_BYTES {
                return Ok(PacketTarget::Header(FRAME_HEADER_BYTES));
            }
            let direction = buffer[6];
            if direction != CAPTURE_DIRECTION && direction != PLAYBACK_DIRECTION {
                return Err(AdapterError::new("media frame direction was malformed"));
            }
            if !matches!(buffer[31], 1 | 2) {
                return Err(AdapterError::new("media frame codec was malformed"));
            }
            let payload_length = u32::from_be_bytes(buffer[38..42].try_into().unwrap()) as usize;
            if payload_length == 0 || payload_length > max_payload_bytes {
                return Err(AdapterError::new(
                    "media frame payload was outside its byte bound",
                ));
            }
            Ok(PacketTarget::Packet(FRAME_HEADER_BYTES + payload_length))
        }
        PLAYBACK_ACK_KIND => Ok(PacketTarget::Packet(MEDIA_PREFIX_BYTES + 17)),
        FLUSH_ACK_KIND => Ok(PacketTarget::Packet(MEDIA_PREFIX_BYTES + 16)),
        _ => Err(AdapterError::new("media frame was malformed")),
    }
}

fn parse_media_packet(
    packet: &[u8],
    version: MediaVersion,
    max_payload_bytes: usize,
) -> Result<MediaMessage, AdapterError> {
    if version
        .timed_prefix()
        .is_some_and(|prefix| prefix == &packet[..5])
    {
        return parse_timed_media_body(packet[5], &packet[TIMED_HEADER_BYTES..], max_payload_bytes);
    }
    match packet[5] {
        FRAME_KIND => parse_frame_body(packet),
        PLAYBACK_ACK_KIND => parse_playback_ack(&packet[MEDIA_PREFIX_BYTES..]),
        FLUSH_ACK_KIND => parse_flush_ack(&packet[MEDIA_PREFIX_BYTES..]),
        _ => Err(AdapterError::new("media frame was malformed")),
    }
}

fn parse_timed_media_body(
    kind: u8,
    body: &[u8],
    max_payload: usize,
) -> Result<MediaMessage, AdapterError> {
    let invalid = || AdapterError::new("timed media record was malformed");
    if kind == CAPTURE_FINISH_KIND {
        let boundary = decode_capture_finish_body(body)?;
        return Ok(MediaMessage::Timed {
            session_id: boundary.session_id(),
            record: TimedMediaRecord::Finish(boundary),
        });
    }
    let session_id = SessionId::new(u64::from_be_bytes(body[..8].try_into().unwrap()));
    let record = match kind {
        2 => {
            if body.len() < 107 {
                return Err(invalid());
            }
            let codec = match body[8] {
                1 => PcmSampleFormat::Signed16LittleEndian,
                2 => PcmSampleFormat::Float32LittleEndian,
                _ => return Err(invalid()),
            };
            let format = PcmFormat::new(
                u32::from_be_bytes(body[9..13].try_into().unwrap()),
                u16::from_be_bytes(body[13..15].try_into().unwrap()),
                codec,
            )?;
            let timing = FrameProvenance::decode(&body[15..15 + CAPTURE_TIMING_BYTES])?;
            let size = u32::from_be_bytes(body[103..107].try_into().unwrap()) as usize;
            if size == 0 || size > max_payload || body.len() != 107 + size {
                return Err(invalid());
            }
            timing.validate_pcm(format, size)?;
            TimedMediaRecord::Frame {
                format,
                timing,
                payload: body[107..].to_vec(),
            }
        }
        6 => TimedMediaRecord::Metadata(CaptureMetadata::Epoch(CaptureEpoch::decode(&body[8..])?)),
        7 => TimedMediaRecord::Metadata(CaptureMetadata::ClockPair(CaptureClockPair::decode(
            &body[8..],
        )?)),
        8 if body.len() == 17 => {
            let capture_epoch = u64::from_be_bytes(body[8..16].try_into().unwrap());
            if capture_epoch == 0 {
                return Err(invalid());
            }
            TimedMediaRecord::Metadata(CaptureMetadata::Discontinuity {
                capture_epoch,
                reason: CaptureDiscontinuityReason::decode(body[16])?,
            })
        }
        _ => return Err(invalid()),
    };
    Ok(MediaMessage::Timed { session_id, record })
}

/// Decodes the exact 41-byte capture finish body shared by kinds 10 and 11.
fn decode_capture_finish_body(body: &[u8]) -> Result<CaptureFinishBoundary, AdapterError> {
    let invalid = || AdapterError::new("capture finish record was malformed");
    if body.len() != CAPTURE_FINISH_BODY_BYTES {
        return Err(invalid());
    }
    let session_id = u64::from_be_bytes(body[..8].try_into().unwrap());
    let capture_epoch = u64::from_be_bytes(body[8..16].try_into().unwrap());
    let frame_present = match body[16] {
        0 => false,
        1 => true,
        _ => return Err(invalid()),
    };
    let last_sequence = u64::from_be_bytes(body[17..25].try_into().unwrap());
    let output_end = u64::from_be_bytes(body[25..33].try_into().unwrap());
    let final_pair_sequence = u64::from_be_bytes(body[33..41].try_into().unwrap());
    if session_id == 0 {
        return Err(invalid());
    }
    CaptureFinishBoundary::new(
        SessionId::new(session_id),
        capture_epoch,
        frame_present,
        last_sequence,
        output_end,
        final_pair_sequence,
    )
    .map_err(|_| invalid())
}

fn encode_capture_finish_body(boundary: &CaptureFinishBoundary) -> [u8; CAPTURE_FINISH_BODY_BYTES] {
    let (last_sequence, output_end) = boundary.frame().unwrap_or((0, 0));
    let mut body = [0_u8; CAPTURE_FINISH_BODY_BYTES];
    body[..8].copy_from_slice(&boundary.session_id().get().to_be_bytes());
    body[8..16].copy_from_slice(&boundary.capture_epoch().to_be_bytes());
    body[16] = u8::from(boundary.frame().is_some());
    body[17..25].copy_from_slice(&last_sequence.to_be_bytes());
    body[25..33].copy_from_slice(&output_end.to_be_bytes());
    body[33..41].copy_from_slice(&boundary.final_clock_pair_sequence().to_be_bytes());
    body
}

/// Writes and flushes the exact kind 11 acknowledgement echoing `boundary`.
/// Called only after the recorder confirmed that boundary.
async fn write_capture_finish_ack<W: AsyncWrite + Unpin>(
    writer: &mut W,
    boundary: &CaptureFinishBoundary,
) -> Result<(), AdapterError> {
    let mut packet = Vec::with_capacity(TIMED_HEADER_BYTES + CAPTURE_FINISH_BODY_BYTES);
    packet.extend_from_slice(b"MED3\x03");
    packet.push(CAPTURE_FINISH_ACK_KIND);
    packet.extend_from_slice(&(CAPTURE_FINISH_BODY_BYTES as u32).to_be_bytes());
    packet.extend_from_slice(&encode_capture_finish_body(boundary));
    writer
        .write_all(&packet)
        .await
        .map_err(|_| AdapterError::new("capture finish acknowledgement write failed"))?;
    writer
        .flush()
        .await
        .map_err(|_| AdapterError::new("capture finish acknowledgement flush failed"))
}

/// One owned in-progress packet read. Bytes already consumed from the socket
/// stay buffered here when a competing `select!` branch wins, so a resumed
/// read continues the same packet instead of resetting mid-packet. Only
/// dropping the reader at connection termination discards them. The idle
/// packet timeout starts at a packet's first byte and is absolute across
/// resumptions.
struct MediaPacketReader<R> {
    stream: R,
    version: MediaVersion,
    max_payload_bytes: usize,
    packet_read_timeout: Duration,
    buffer: Vec<u8>,
    deadline: Option<tokio::time::Instant>,
}

impl<R: AsyncRead + Unpin> MediaPacketReader<R> {
    fn new(
        stream: R,
        version: MediaVersion,
        max_payload_bytes: usize,
        packet_read_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            version,
            max_payload_bytes,
            packet_read_timeout,
            buffer: Vec::new(),
            deadline: None,
        }
    }

    /// `Ok(None)` is a clean EOF at a packet boundary; EOF inside a packet is
    /// malformed input like any other truncation.
    async fn next_message(&mut self) -> Result<Option<MediaMessage>, AdapterError> {
        loop {
            let goal = match packet_target(&self.buffer, self.version, self.max_payload_bytes)? {
                PacketTarget::Packet(length) if self.buffer.len() >= length => {
                    let packet = std::mem::take(&mut self.buffer);
                    self.deadline = None;
                    return parse_media_packet(&packet, self.version, self.max_payload_bytes)
                        .map(Some);
                }
                PacketTarget::Packet(length) | PacketTarget::Header(length) => length,
            };
            let mut chunk = [0_u8; 4_096];
            let wanted = (goal - self.buffer.len()).min(chunk.len());
            let read = match self.deadline {
                None => self.stream.read(&mut chunk[..wanted]).await,
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, self.stream.read(&mut chunk[..wanted]))
                        .await
                        .map_err(|_| AdapterError::new("media bridge packet read timed out"))?
                }
            };
            match read {
                Ok(0) if self.buffer.is_empty() => return Ok(None),
                Ok(0) | Err(_) => return Err(AdapterError::new("media frame was malformed")),
                Ok(count) => {
                    if self.deadline.is_none() {
                        self.deadline =
                            Some(tokio::time::Instant::now() + self.packet_read_timeout);
                    }
                    self.buffer.extend_from_slice(&chunk[..count]);
                }
            }
        }
    }
}

fn parse_frame_body(packet: &[u8]) -> Result<MediaMessage, AdapterError> {
    let header = &packet[..FRAME_HEADER_BYTES];
    let direction = header[6];
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
    let format = PcmFormat::new(sample_rate_hz, channels, sample_format)?;
    Ok(MediaMessage::Frame {
        direction,
        session_id,
        sequence,
        captured_at_ms,
        format,
        payload: packet[FRAME_HEADER_BYTES..].to_vec(),
    })
}

fn parse_playback_ack(body: &[u8]) -> Result<MediaMessage, AdapterError> {
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

fn parse_flush_ack(body: &[u8]) -> Result<MediaMessage, AdapterError> {
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

#[cfg(test)]
mod timing_domain_tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::ReadBuf;

    #[derive(Clone, Copy, Debug)]
    enum AckFault {
        WriteStall,
        FlushStall,
        PartialWriteError,
        FlushError,
        None,
    }

    struct AckIo {
        input: std::io::Cursor<Vec<u8>>,
        read_bytes: Arc<std::sync::atomic::AtomicUsize>,
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        entered: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
        delay: Pin<Box<tokio::time::Sleep>>,
        flush_release: Option<tokio::sync::oneshot::Receiver<()>>,
        fault: AckFault,
    }
    impl Drop for AckIo {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl AsyncRead for AckIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            let position = self.input.position() as usize;
            let count = out.remaining().min(self.input.get_ref().len() - position);
            out.put_slice(&self.input.get_ref()[position..position + count]);
            self.input.set_position((position + count) as u64);
            self.read_bytes.fetch_add(count, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for AckIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.entered.notify_one();
            if matches!(self.fault, AckFault::WriteStall) {
                return Poll::Pending;
            }
            let mut written = self.written.lock().unwrap();
            if matches!(self.fault, AckFault::PartialWriteError) && !written.is_empty() {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let count = if matches!(self.fault, AckFault::PartialWriteError) {
                bytes.len().min(3)
            } else {
                bytes.len()
            };
            written.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            match self.fault {
                AckFault::FlushStall => match self.flush_release.as_mut() {
                    Some(release) => Pin::new(release).poll(cx).map(|_| Ok(())),
                    None => Poll::Pending,
                },
                AckFault::FlushError => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
                _ => Poll::Ready(Ok(())),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    fn ack_io(lease: &MediaLease, delay: Duration, fault: AckFault) -> (AckIo, usize) {
        let mut body = lease.session_id().get().to_be_bytes().to_vec();
        body.push(lease.credential().len() as u8);
        body.extend(lease.credential().as_bytes());
        body.push(18);
        body.extend(b"bridge-independent");
        let mut input = b"MED2\x02\x01".to_vec();
        input.extend((body.len() as u32).to_be_bytes());
        input.extend(body);
        let auth_len = input.len();
        // Deliberately queued early. It must not be read/admitted on ACK failure.
        input.extend_from_slice(b"MED2\x02\x06\x00\x00\x00\x00");
        (
            AckIo {
                input: std::io::Cursor::new(input),
                read_bytes: Arc::default(),
                written: Arc::default(),
                entered: Arc::default(),
                dropped: Arc::default(),
                delay: Box::pin(tokio::time::sleep(delay)),
                flush_release: None,
                fault,
            },
            auth_len,
        )
    }
    async fn timed_server() -> MediaServer {
        MediaServer::bind(
            MediaServerConfig::loopback()
                .with_capture_format(Some("pcm_s16le/24000/1/20ms".into()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn timed_ack_stall_shares_read_deadline_and_releases_exact_connection() {
        for fault in [AckFault::WriteStall, AckFault::FlushStall] {
            let server = timed_server().await;
            let id = SessionId::new(41);
            let lease = server
                .reserve_timed(id, "bridge-independent".into())
                .await
                .unwrap();
            let (io, auth_len) = ack_io(&lease, Duration::from_millis(1500), fault);
            let entered = io.entered.clone();
            let read = io.read_bytes.clone();
            let dropped = io.dropped.clone();
            let origin = tokio::time::Instant::now();
            let task = tokio::spawn(serve_connection(io, server.inner.clone()));
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(1500)).await;
            tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, entered.notified())
                .await
                .expect("ACK writer was never entered");
            assert_eq!(read.load(Ordering::SeqCst), auth_len);
            let hub = server.hub();
            let frame = frame_for_network_capture(
                1,
                PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                vec![0, 0],
            )
            .unwrap();
            let mut playback = Box::pin(hub.enqueue_playback(id, frame, CancellationToken::new()));
            std::future::poll_fn(|cx| {
                assert!(
                    matches!(playback.as_mut().poll(cx), Poll::Ready(Err(_))),
                    "playback attached before ACK flush"
                );
                Poll::Ready(())
            })
            .await;
            let old = {
                let sessions = server.inner.sessions.lock().await;
                let state = &sessions[&id.get()];
                assert!(state.active);
                MediaConnection {
                    reservation_epoch: state.reservation_epoch,
                    connection_id: state.active_connection_id.unwrap(),
                }
            };
            tokio::time::advance(Duration::from_millis(499)).await;
            assert!(!task.is_finished());
            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(task.await.unwrap().is_err());
            assert_eq!(
                tokio::time::Instant::now() - origin,
                MEDIA_HANDSHAKE_TIMEOUT
            );
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(
                read.load(Ordering::SeqCst),
                auth_len,
                "ACK failure admitted media"
            );
            assert!(!server.inner.sessions.lock().await[&id.get()].active);
            let replacement = server
                .inner
                .authenticate(
                    id,
                    lease.credential(),
                    Some("bridge-independent"),
                    MediaVersion::Timed,
                )
                .await
                .unwrap();
            assert!(server.inner.disconnect(id, old).await.is_none());
            assert_eq!(
                server.inner.sessions.lock().await[&id.get()].active_connection_id,
                Some(replacement.connection_id)
            );
            server.inner.disconnect(id, replacement).await.unwrap();
            server.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timed_ack_partial_write_flush_error_and_cancellation_cleanup_are_owned() {
        for fault in [
            AckFault::PartialWriteError,
            AckFault::FlushError,
            AckFault::WriteStall,
            AckFault::FlushStall,
        ] {
            let server = timed_server().await;
            let id = SessionId::new(41);
            let lease = server
                .reserve_timed(id, "bridge-independent".into())
                .await
                .unwrap();
            let (io, auth_len) = ack_io(&lease, Duration::ZERO, fault);
            let entered = io.entered.clone();
            let read = io.read_bytes.clone();
            let dropped = io.dropped.clone();
            let written = io.written.clone();
            let task = tokio::spawn(serve_connection(io, server.inner.clone()));
            tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, entered.notified())
                .await
                .expect("ACK writer was never entered");
            let stalled = matches!(fault, AckFault::WriteStall | AckFault::FlushStall);
            if stalled {
                server.inner.cancellation.cancel();
            }
            let result = task.await.unwrap();
            assert_eq!(result.is_ok(), stalled);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(read.load(Ordering::SeqCst), auth_len);
            assert!(!server.inner.sessions.lock().await[&id.get()].active);
            if matches!(fault, AckFault::PartialWriteError) {
                assert_eq!(written.lock().unwrap().len(), 3);
            }
            assert_eq!(
                server.inner.sessions.lock().await[&id.get()]
                    .timed
                    .as_ref()
                    .unwrap()
                    .metadata_in_window,
                0
            );
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn timed_ack_canonical_bytes_and_attachment_failure_release_active_state() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/timed-capture-v1/metadata.json"
        ))
        .unwrap();
        let mut bytes = Vec::new();
        write_authentication_accepted(
            &mut bytes,
            SessionId::new(41),
            "bridge-independent",
            MediaVersion::Timed,
        )
        .await
        .unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, fixture["authenticationAcceptedHex"]);
        assert!(!bytes
            .windows(b"fixture-token".len())
            .any(|part| part == b"fixture-token"));
        for bridge in ["", "bad\nbridge", &"x".repeat(65)] {
            assert!(write_authentication_accepted(
                &mut Vec::new(),
                SessionId::new(41),
                bridge,
                MediaVersion::Timed
            )
            .await
            .is_err());
        }
        let server = timed_server().await;
        let id = SessionId::new(41);
        let lease = server
            .reserve_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        server.hub().revoke(id).await; // Auth registry still valid; playback attach fails.
        let (io, auth_len) = ack_io(&lease, Duration::ZERO, AckFault::None);
        let read = io.read_bytes.clone();
        let written = io.written.clone();
        assert!(serve_connection(io, server.inner.clone()).await.is_err());
        assert_eq!(*written.lock().unwrap(), bytes);
        assert_eq!(read.load(Ordering::SeqCst), auth_len);
        assert!(!server.inner.sessions.lock().await[&id.get()].active);
        server.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn timed_ack_revoke_while_stalled_is_bounded_and_cannot_release_new_reservation() {
        let server = timed_server().await;
        let id = SessionId::new(41);
        let lease = server
            .reserve_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        let (io, _) = ack_io(&lease, Duration::ZERO, AckFault::FlushStall);
        let entered = io.entered.clone();
        let dropped = io.dropped.clone();
        let task = tokio::spawn(serve_connection(io, server.inner.clone()));
        tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, entered.notified())
            .await
            .expect("ACK writer was never entered");
        server.revoke(id).await;
        let lease = server
            .reserve_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        let replacement = server
            .inner
            .authenticate(
                id,
                lease.credential(),
                Some("bridge-independent"),
                MediaVersion::Timed,
            )
            .await
            .unwrap();
        tokio::time::advance(MEDIA_HANDSHAKE_TIMEOUT).await;
        assert!(task.await.unwrap().is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(
            server.inner.sessions.lock().await[&id.get()].active_connection_id,
            Some(replacement.connection_id)
        );
        server.inner.disconnect(id, replacement).await.unwrap();
        server.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn timed_ack_shutdown_joins_the_registered_stalled_connection_owner() {
        let server = timed_server().await;
        let id = SessionId::new(41);
        let lease = server
            .reserve_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        let (io, _) = ack_io(&lease, Duration::ZERO, AckFault::FlushStall);
        let entered = io.entered.clone();
        let dropped = io.dropped.clone();
        let inner = server.inner.clone();
        let task = tokio::spawn(async move {
            let _ = serve_connection(io, inner).await;
        });
        server.inner.connections.lock().await.push(task);
        tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, entered.notified())
            .await
            .expect("ACK writer was never entered");
        tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, server.shutdown())
            .await
            .expect("shutdown failed to join ACK owner");
        assert!(dropped.load(Ordering::SeqCst));
        assert!(server.inner.connections.lock().await.is_empty());
        assert!(!server.inner.sessions.lock().await[&id.get()].active);
    }

    #[tokio::test]
    async fn timed_ack_failed_reconnect_does_not_republish_prior_capture_gap() {
        use conversation_model_adapters::{NetworkVoiceIoFactory, VoiceIoFactory};
        let server = timed_server().await;
        let id = SessionId::new(41);
        let lease = server
            .reserve_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let factory = NetworkVoiceIoFactory::new(server.hub(), id);
        let io = factory.start(id, cancellation.clone()).await.unwrap();
        let mut recording = io.input.start(id, cancellation.clone()).await.unwrap();
        let capture = factory.recognition_capture();
        let mut recognition = capture.start(id, cancellation.clone()).await.unwrap();
        // Persisted capture state from the preceding connection, whose real
        // disconnect has already been reported. This attempt admits no media.
        server
            .inner
            .admit_epoch(
                id,
                &CaptureEpoch {
                    bridge_session_id: "bridge-independent".into(),
                    clock_domain_id: "previous-process".into(),
                    capture_epoch: 3,
                },
                false,
            )
            .await
            .unwrap();
        server
            .inner
            .sessions
            .lock()
            .await
            .get_mut(&id.get())
            .unwrap()
            .timed
            .as_mut()
            .unwrap()
            .sequence = 7;
        let (stream, auth_len) = ack_io(&lease, Duration::ZERO, AckFault::PartialWriteError);
        let read = stream.read_bytes.clone();
        let result = serve_connection(stream, server.inner.clone()).await;
        let recorded = recording.try_recv();
        let recognized = recognition.try_recv();
        cancellation.cancel();
        io.completion.await.unwrap().unwrap();
        server.shutdown().await;
        assert!(result.is_err());
        assert_eq!(read.load(Ordering::SeqCst), auth_len);
        assert!(
            recorded.is_err(),
            "failed confirmation republished old recording event: {recorded:?}"
        );
        assert!(
            recognized.is_err(),
            "failed confirmation fabricated recognizer loss: {recognized:?}"
        );
    }

    #[tokio::test]
    async fn timed_ack_flush_release_cannot_attach_or_read_into_replacement_reservation() {
        for already_attached in [false, true] {
            let server = timed_server().await;
            let id = SessionId::new(41);
            let lease = server
                .reserve_timed(id, "bridge-independent".into())
                .await
                .unwrap();
            let (mut stream, auth_len) = ack_io(&lease, Duration::ZERO, AckFault::FlushStall);
            let entered = stream.entered.clone();
            let read = stream.read_bytes.clone();
            let (release, released) = tokio::sync::oneshot::channel();
            stream.flush_release = Some(released);
            let old = tokio::spawn(serve_connection(stream, server.inner.clone()));
            tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, entered.notified())
                .await
                .unwrap();
            server.revoke(id).await;
            let lease = server
                .reserve_timed(id, "bridge-independent".into())
                .await
                .unwrap();
            let replacement = server
                .inner
                .authenticate(
                    id,
                    lease.credential(),
                    Some("bridge-independent"),
                    MediaVersion::Timed,
                )
                .await
                .unwrap();
            let mut transport = if already_attached {
                Some(
                    server
                        .hub()
                        .attach_playback_transport(id, replacement.connection_id)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            release.send(()).unwrap();
            let result = tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, old)
                .await
                .unwrap()
                .unwrap();
            let replacement_attach = if transport.is_none() {
                Some(
                    server
                        .hub()
                        .attach_playback_transport(id, replacement.connection_id)
                        .await,
                )
            } else {
                None
            };
            let untouched = transport.as_mut().is_none_or(|transport| {
                matches!(
                    transport.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                )
            });
            let active = server.inner.sessions.lock().await[&id.get()].active_connection_id;
            server.shutdown().await;
            assert!(result.is_err());
            assert_eq!(
                read.load(Ordering::SeqCst),
                auth_len,
                "old ACK completion read replacement media"
            );
            assert!(
                replacement_attach.is_none_or(|result| result.is_ok()),
                "old ACK completion stole replacement attachment"
            );
            assert!(untouched);
            assert_eq!(active, Some(replacement.connection_id));
        }
    }

    #[tokio::test]
    async fn timed_cleanup_fences_new_epoch_until_old_fanout_settles_and_cancels_without_overtaking(
    ) {
        use conversation_model_adapters::{
            CaptureEvent, NetworkVoiceIoFactory, VoiceInputEvent, VoiceIoFactory,
        };
        for action in ["drain", "cancel", "replace"] {
            let server = timed_server().await;
            let id = SessionId::new(41);
            let lease = server
                .reserve_timed(id, "bridge-independent".into())
                .await
                .unwrap();
            let cancellation = CancellationToken::new();
            let factory = NetworkVoiceIoFactory::new(server.hub(), id);
            let io = factory.start(id, cancellation.clone()).await.unwrap();
            let mut recording = io.input.start(id, cancellation.clone()).await.unwrap();
            let capture = factory.recognition_capture();
            let mut recognition = capture.start(id, cancellation.clone()).await.unwrap();
            let connection = server
                .inner
                .authenticate(
                    id,
                    lease.credential(),
                    Some("bridge-independent"),
                    MediaVersion::Timed,
                )
                .await
                .unwrap();
            let mut old_transport = server
                .hub()
                .attach_playback_transport(id, connection.connection_id)
                .await
                .unwrap();
            server
                .inner
                .admit_epoch(
                    id,
                    &CaptureEpoch {
                        bridge_session_id: "bridge-independent".into(),
                        clock_domain_id: "old".into(),
                        capture_epoch: 3,
                    },
                    false,
                )
                .await
                .unwrap();
            server
                .inner
                .sessions
                .lock()
                .await
                .get_mut(&id.get())
                .unwrap()
                .timed
                .as_mut()
                .unwrap()
                .sequence = 7;
            for sequence in 1..=16 {
                server
                    .hub()
                    .deliver_capture(
                        id,
                        frame_for_network_capture(
                            sequence,
                            PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian)
                                .unwrap(),
                            vec![0, 0],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
            let started = Instant::now();
            let mut cleanup = Box::pin(cleanup_connection(&server.inner, id, connection, true));
            std::future::poll_fn(|cx| {
                assert!(cleanup.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let blocked = Instant::now();
            assert!(matches!(
                old_transport.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ));
            assert!(
                server
                    .inner
                    .authenticate(
                        id,
                        lease.credential(),
                        Some("bridge-independent"),
                        MediaVersion::Timed
                    )
                    .await
                    .is_err(),
                "new epoch could overtake pending old gap"
            );
            let mut replacement = None;
            if action == "replace" {
                server.revoke(id).await;
                let lease = server
                    .reserve_timed(id, "bridge-independent".into())
                    .await
                    .unwrap();
                let factory = NetworkVoiceIoFactory::new(server.hub(), id);
                let io = factory.start(id, cancellation.clone()).await.unwrap();
                let recording = io.input.start(id, cancellation.clone()).await.unwrap();
                let capture = factory.recognition_capture();
                let recognition = capture.start(id, cancellation.clone()).await.unwrap();
                let connection = server
                    .inner
                    .authenticate(
                        id,
                        lease.credential(),
                        Some("bridge-independent"),
                        MediaVersion::Timed,
                    )
                    .await
                    .unwrap();
                let transport = server
                    .hub()
                    .attach_playback_transport(id, connection.connection_id)
                    .await
                    .unwrap();
                server
                    .inner
                    .admit_epoch(
                        id,
                        &CaptureEpoch {
                            bridge_session_id: "bridge-independent".into(),
                            clock_domain_id: "new".into(),
                            capture_epoch: 99,
                        },
                        false,
                    )
                    .await
                    .unwrap();
                replacement = Some((io, recording, recognition, transport, connection));
            }
            if action == "cancel" {
                server.inner.cancellation.cancel();
            }
            for _ in 0..16 {
                recording.recv().await.unwrap().unwrap();
                recognition.recv().await.unwrap().unwrap();
            }
            tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, cleanup)
                .await
                .unwrap();
            if action == "cancel" {
                assert!(recording.try_recv().is_err());
                assert!(recognition.try_recv().is_err());
            } else {
                assert!(matches!(
                    recording.try_recv().unwrap().unwrap(),
                    VoiceInputEvent::Capture(CaptureEvent::Discontinuity { .. })
                ));
                assert!(matches!(
                    recognition.try_recv().unwrap().unwrap(),
                    CaptureEvent::Discontinuity { .. }
                ));
                assert_eq!(
                    recording.try_recv().unwrap().unwrap(),
                    VoiceInputEvent::CaptureMetadata(CaptureMetadata::Discontinuity {
                        capture_epoch: 3,
                        reason: CaptureDiscontinuityReason::Reconnect
                    })
                );
            }
            if let Some((
                new_io,
                mut new_recording,
                mut new_recognition,
                mut transport,
                new_connection,
            )) = replacement
            {
                assert!(new_recording.try_recv().is_err());
                assert!(new_recognition.try_recv().is_err());
                assert!(matches!(
                    transport.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(
                    server.inner.sessions.lock().await[&id.get()].active_connection_id,
                    Some(new_connection.connection_id)
                );
                assert_eq!(
                    server.inner.sessions.lock().await[&id.get()]
                        .timed
                        .as_ref()
                        .unwrap()
                        .epoch
                        .as_ref()
                        .unwrap()
                        .capture_epoch,
                    99
                );
                cancellation.cancel();
                new_io.completion.await.unwrap().unwrap();
            } else {
                let sessions = server.inner.sessions.lock().await;
                let state = &sessions[&id.get()];
                assert!(!state.active);
                assert!(state.reconnect_deadline >= started + server.inner.reconnect_window);
                assert!(
                    state.reconnect_deadline <= blocked + server.inner.reconnect_window,
                    "cleanup extended original reconnect deadline"
                );
            }
            cancellation.cancel();
            io.completion.await.unwrap().unwrap();
            server.shutdown().await;
        }
    }
    #[test]
    fn review_rate_windows_admit_exact_caps_and_reset_independently_at_one_second() {
        let origin = Instant::now();
        let mut pcm_started = origin;
        let mut metadata_started = origin + Duration::from_millis(250);
        let (mut pcm, mut metadata) = (0, 0);
        for _ in 0..2 {
            admit_rate_window(&mut pcm_started, &mut pcm, 2, metadata_started).unwrap();
        }
        for _ in 0..128 {
            admit_rate_window(
                &mut metadata_started,
                &mut metadata,
                128,
                origin + Duration::from_millis(250),
            )
            .unwrap();
        }
        assert!(admit_rate_window(
            &mut pcm_started,
            &mut pcm,
            2,
            origin + Duration::from_secs(1) - Duration::from_nanos(1)
        )
        .is_err());
        admit_rate_window(
            &mut pcm_started,
            &mut pcm,
            2,
            origin + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(pcm, 1);
        assert!(admit_rate_window(
            &mut metadata_started,
            &mut metadata,
            128,
            origin + Duration::from_secs(1)
        )
        .is_err());
        assert_eq!(metadata, 128);
        assert!(admit_rate_window(
            &mut metadata_started,
            &mut metadata,
            128,
            origin + Duration::from_millis(1250) - Duration::from_nanos(1)
        )
        .is_err());
        admit_rate_window(
            &mut metadata_started,
            &mut metadata,
            128,
            origin + Duration::from_millis(1250),
        )
        .unwrap();
        assert_eq!(metadata, 1);
        assert_eq!(pcm, 1);
    }

    #[tokio::test]
    async fn review_timed_rate_budgets_survive_reconnect_epoch_and_domain_changes() {
        let server = MediaServer::bind(
            MediaServerConfig::loopback()
                .with_max_frames_per_second(2)
                .with_capture_format(Some("pcm_s16le/24000/1/20ms".into()))
                .unwrap(),
        )
        .await
        .unwrap();
        let id = SessionId::new(41);
        let lease = server.reserve_timed(id, "bridge".into()).await.unwrap();
        let epoch = |domain: &str, number| CaptureEpoch {
            bridge_session_id: "bridge".into(),
            clock_domain_id: domain.into(),
            capture_epoch: number,
        };
        let connection = server
            .inner
            .authenticate(id, lease.credential(), Some("bridge"), MediaVersion::Timed)
            .await
            .unwrap();
        server.inner.admit_timed_rate(id, true).await.unwrap();
        server
            .inner
            .admit_epoch(id, &epoch("A", 1), false)
            .await
            .unwrap();
        for _ in 1..64 {
            server.inner.admit_timed_rate(id, true).await.unwrap();
        }
        for _ in 0..2 {
            server.inner.admit_timed_rate(id, false).await.unwrap();
        }
        assert!(server.inner.admit_timed_rate(id, false).await.is_err());
        server.inner.disconnect(id, connection).await.unwrap();
        let reconnected = server
            .inner
            .authenticate(id, lease.credential(), Some("bridge"), MediaVersion::Timed)
            .await
            .unwrap();
        for (domain, number, reconnect) in [("A", 1, true), ("A", 2, false), ("B", 1, false)] {
            server.inner.admit_timed_rate(id, true).await.unwrap();
            server
                .inner
                .admit_epoch(id, &epoch(domain, number), reconnect)
                .await
                .unwrap();
        }
        for _ in 67..128 {
            server.inner.admit_timed_rate(id, true).await.unwrap();
        }
        assert!(
            server.inner.admit_timed_rate(id, true).await.is_err(),
            "129th metadata record must fail"
        );
        assert!(
            server.inner.admit_timed_rate(id, false).await.is_err(),
            "reconnect must not reset PCM budget"
        );
        {
            let sessions = server.inner.sessions.lock().await;
            let session = &sessions[&id.get()];
            assert_eq!(session.frames_in_window, 2);
            assert_eq!(session.timed.as_ref().unwrap().metadata_in_window, 128);
        }
        server.inner.disconnect(id, reconnected).await.unwrap();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn domain_history_is_per_source_persists_reconnect_and_refuses_257_without_resetting_watermarks(
    ) {
        let server = MediaServer::bind(
            MediaServerConfig::loopback()
                .with_capture_format(Some("pcm_s16le/24000/1/20ms".into()))
                .unwrap(),
        )
        .await
        .unwrap();
        let first = SessionId::new(41);
        let second = SessionId::new(42);
        server.reserve_timed(first, "bridge".into()).await.unwrap();
        server.reserve_timed(second, "bridge".into()).await.unwrap();
        let epoch = |name: &str| CaptureEpoch {
            bridge_session_id: "bridge".into(),
            clock_domain_id: name.into(),
            capture_epoch: 1,
        };
        server
            .inner
            .admit_epoch(first, &epoch("A"), false)
            .await
            .unwrap();
        server
            .inner
            .admit_epoch(first, &epoch("A"), true)
            .await
            .unwrap();
        server
            .inner
            .admit_epoch(first, &epoch("B"), false)
            .await
            .unwrap();
        assert!(server
            .inner
            .admit_epoch(first, &epoch("A"), true)
            .await
            .is_err());
        server
            .inner
            .admit_epoch(second, &epoch("A"), false)
            .await
            .unwrap();
        for index in 1..256 {
            server
                .inner
                .admit_epoch(second, &epoch(&format!("process-{index}")), false)
                .await
                .unwrap();
        }
        {
            let mut sessions = server.inner.sessions.lock().await;
            let state = sessions
                .get_mut(&second.get())
                .unwrap()
                .timed
                .as_mut()
                .unwrap();
            state.sequence = 9;
            state.output_end = Some(10);
        }
        assert!(server
            .inner
            .admit_epoch(second, &epoch("process-256"), false)
            .await
            .is_err());
        server
            .inner
            .admit_epoch(second, &epoch("process-255"), true)
            .await
            .unwrap();
        {
            let sessions = server.inner.sessions.lock().await;
            let state = sessions[&second.get()].timed.as_ref().unwrap();
            assert_eq!(state.sequence, 9);
            assert_eq!(state.output_end, Some(10));
            assert_eq!(state.epoch.as_ref().unwrap().clock_domain_id, "process-255");
        }
        server.shutdown().await;
    }

    fn finish_body(
        session_id: u64,
        capture_epoch: u64,
        frame_present: u8,
        sequence: u64,
        output_end: u64,
        pair_sequence: u64,
    ) -> Vec<u8> {
        let mut body = Vec::with_capacity(CAPTURE_FINISH_BODY_BYTES);
        body.extend_from_slice(&session_id.to_be_bytes());
        body.extend_from_slice(&capture_epoch.to_be_bytes());
        body.push(frame_present);
        body.extend_from_slice(&sequence.to_be_bytes());
        body.extend_from_slice(&output_end.to_be_bytes());
        body.extend_from_slice(&pair_sequence.to_be_bytes());
        body
    }

    #[tokio::test]
    async fn drainable_finish_codec_accepts_exact_nonempty_and_zero_frame_boundaries() {
        for (present, sequence, output_end) in [(1, 7, 960), (0, 0, 0)] {
            let body = finish_body(41, 2, present, sequence, output_end, 3);
            let boundary = decode_capture_finish_body(&body).unwrap();
            assert_eq!(boundary.session_id(), SessionId::new(41));
            assert_eq!(boundary.capture_epoch(), 2);
            assert_eq!(
                boundary.frame(),
                (present == 1).then_some((sequence, output_end))
            );
            assert_eq!(boundary.final_clock_pair_sequence(), 3);
        }
    }

    #[tokio::test]
    async fn drainable_finish_codec_rejects_noncanonical_or_truncated_boundaries() {
        for body in [
            finish_body(41, 2, 2, 7, 960, 3),
            finish_body(41, 2, 0, 7, 960, 3),
            finish_body(41, 0, 1, 7, 960, 3),
            finish_body(41, 2, 1, 7, 960, 0),
            finish_body(41, 2, 1, 7, 960, 3)[..40].to_vec(),
        ] {
            assert!(decode_capture_finish_body(&body).is_err());
        }
    }

    #[tokio::test]
    async fn drainable_finish_ack_is_exact_and_flushed() {
        let boundary = CaptureFinishBoundary::new(SessionId::new(41), 2, true, 7, 960, 3).unwrap();
        let mut output = Vec::new();
        write_capture_finish_ack(&mut output, &boundary)
            .await
            .unwrap();
        let mut expected = b"MED3\x03\x0b\x00\x00\x00\x29".to_vec();
        expected.extend(finish_body(41, 2, 1, 7, 960, 3));
        assert_eq!(output, expected);
        assert_eq!(output.len(), 51);
    }

    #[tokio::test]
    async fn timed_and_drainable_envelopes_are_not_interchangeable() {
        let body = finish_body(41, 2, 1, 7, 960, 3);
        let mut media3 = b"MED3\x03\x0a\x00\x00\x00\x29".to_vec();
        media3.extend(body);
        assert!(MediaPacketReader::new(
            std::io::Cursor::new(media3),
            MediaVersion::Timed,
            MAX_PCM_FRAME_BYTES,
            Duration::from_secs(1),
        )
        .next_message()
        .await
        .is_err());

        let media2 = b"MED2\x02\x06\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00\x29";
        assert!(MediaPacketReader::new(
            std::io::Cursor::new(media2),
            MediaVersion::DrainableTimed,
            MAX_PCM_FRAME_BYTES,
            Duration::from_secs(1),
        )
        .next_message()
        .await
        .is_err());
    }

    /// Serves scripted input in chunks. Serving the chunk at `gate_before`
    /// waits for the gate, returning `Pending` so a competing `select!`
    /// branch can win while a packet is only partly consumed.
    struct FragmentIo {
        chunks: std::collections::VecDeque<Vec<u8>>,
        gate_before: usize,
        served: usize,
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
        stalled: Arc<tokio::sync::Notify>,
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        flushed: Arc<tokio::sync::Notify>,
    }
    impl AsyncRead for FragmentIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.served == self.gate_before {
                if let Some(gate) = self.gate.as_mut() {
                    if Pin::new(gate).poll(cx).is_pending() {
                        self.stalled.notify_one();
                        return Poll::Pending;
                    }
                    self.gate = None;
                }
            }
            let Some(mut chunk) = self.chunks.pop_front() else {
                return Poll::Pending;
            };
            let count = out.remaining().min(chunk.len());
            out.put_slice(&chunk[..count]);
            if count < chunk.len() {
                chunk.drain(..count);
                self.chunks.push_front(chunk);
            } else {
                self.served += 1;
            }
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for FragmentIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            self.flushed.notify_one();
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A playback acknowledgement split across the moment the outbound
    /// playback branch wins the connection `select!`. The bytes already
    /// consumed before the stall must still belong to that acknowledgement.
    #[tokio::test]
    async fn fragmented_acknowledgement_survives_a_competing_playback_branch() {
        use conversation_protocol::PlaybackState;
        let server = MediaServer::bind(MediaServerConfig::loopback())
            .await
            .unwrap();
        let id = SessionId::new(41);
        let lease = server.reserve(id).await.unwrap();
        let mut auth = b"MED1\x01\x01".to_vec();
        auth.extend(id.get().to_be_bytes());
        auth.push(lease.credential().len() as u8);
        auth.extend(lease.credential().as_bytes());
        let mut ack_tail = vec![PLAYBACK_ACK_KIND];
        ack_tail.extend(id.get().to_be_bytes());
        ack_tail.extend(1_u64.to_be_bytes());
        ack_tail.push(1);
        let (release, gate) = tokio::sync::oneshot::channel();
        let io = FragmentIo {
            chunks: [auth, b"MED1\x01".to_vec(), ack_tail].into(),
            gate_before: 2,
            served: 0,
            gate: Some(gate),
            stalled: Arc::default(),
            written: Arc::default(),
            flushed: Arc::default(),
        };
        let stalled = io.stalled.clone();
        let written = io.written.clone();
        let flushed = io.flushed.clone();
        let task = tokio::spawn(serve_connection(io, server.inner.clone()));
        tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, stalled.notified())
            .await
            .expect("reader never stalled mid-packet");
        let hub = server.hub();
        let frame = frame_for_network_capture(
            1,
            PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0, 0],
        )
        .unwrap();
        let playback = tokio::spawn(async move {
            hub.enqueue_playback(id, frame, CancellationToken::new())
                .await
        });
        tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, flushed.notified())
            .await
            .expect("playback frame was never written while the read was stalled");
        assert!(written.lock().unwrap().starts_with(b"MED1\x01\x02\x02"));
        release.send(()).unwrap();
        let receipt = tokio::time::timeout(MEDIA_HANDSHAKE_TIMEOUT, playback)
            .await
            .expect("fragmented acknowledgement was never processed")
            .unwrap();
        assert_eq!(
            receipt
                .expect("partially read acknowledgement was reset by the playback branch")
                .state(),
            PlaybackState::Accepted
        );
        assert!(
            !task.is_finished(),
            "connection failed after the fragmented acknowledgement"
        );
        server.shutdown().await;
        assert!(task.await.unwrap().is_ok());
    }

    fn med3(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut packet = b"MED3\x03".to_vec();
        packet.push(kind);
        packet.extend((body.len() as u32).to_be_bytes());
        packet.extend_from_slice(body);
        packet
    }

    struct PendingFinishFixture {
        peer: tokio::io::DuplexStream,
        task: JoinHandle<Result<(), AdapterError>>,
        recording: tokio::sync::mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        _recognition: tokio::sync::mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
        io: conversation_model_adapters::VoiceIoSession,
        finish: conversation_model_adapters::CaptureFinish,
    }
    use conversation_model_adapters::{CaptureEvent, VoiceInputEvent};

    /// Authenticates a drainable in-process peer, admits an epoch and clock
    /// pair, sends the zero-frame finish and returns once the recorder
    /// barrier is on the voice queue; the test decides how it settles.
    async fn drainable_pending_finish(
        server: &MediaServer,
        cancellation: &CancellationToken,
    ) -> PendingFinishFixture {
        use conversation_model_adapters::{
            CaptureClockPair, CaptureEpoch, NetworkVoiceIoFactory, VoiceIoFactory,
        };
        let id = SessionId::new(41);
        let lease = server
            .reserve_drainable_timed(id, "bridge-independent".into())
            .await
            .unwrap();
        let factory = NetworkVoiceIoFactory::new(server.hub(), id);
        let io = factory.start(id, cancellation.clone()).await.unwrap();
        let mut recording = io.input.start(id, cancellation.clone()).await.unwrap();
        let recognition = factory
            .recognition_capture()
            .start(id, cancellation.clone())
            .await
            .unwrap();
        let (server_side, mut peer) = tokio::io::duplex(65_536);
        let task = tokio::spawn(serve_connection(server_side, server.inner.clone()));
        let mut body = id.get().to_be_bytes().to_vec();
        body.push(lease.credential().len() as u8);
        body.extend(lease.credential().as_bytes());
        body.push(18);
        body.extend(b"bridge-independent");
        peer.write_all(&med3(AUTH_KIND, &body)).await.unwrap();
        let mut confirmation = [0_u8; 37];
        peer.read_exact(&mut confirmation).await.unwrap();
        assert_eq!(&confirmation[..6], b"MED3\x03\x09");
        let epoch = CaptureEpoch {
            bridge_session_id: "bridge-independent".into(),
            clock_domain_id: "source-process".into(),
            capture_epoch: 1,
        };
        let mut body = id.get().to_be_bytes().to_vec();
        body.extend(epoch.encode().unwrap());
        peer.write_all(&med3(6, &body)).await.unwrap();
        assert!(matches!(
            recording.recv().await.unwrap().unwrap(),
            VoiceInputEvent::CaptureMetadata(CaptureMetadata::Epoch(_))
        ));
        let pair = CaptureClockPair {
            clock_domain_id: "source-process".into(),
            wall_epoch: 1,
            sequence: 1,
            host_before_ns: 900_000_000,
            host_after_ns: 900_000_000,
            local_unix_ns: 2_000_000_000,
            wall_resolution_ns: 1,
            external_utc_accuracy_ns: None,
        };
        let mut body = id.get().to_be_bytes().to_vec();
        body.extend(pair.encode().unwrap());
        peer.write_all(&med3(7, &body)).await.unwrap();
        assert!(matches!(
            recording.recv().await.unwrap().unwrap(),
            VoiceInputEvent::CaptureMetadata(CaptureMetadata::ClockPair(_))
        ));
        peer.write_all(&med3(CAPTURE_FINISH_KIND, &finish_body(41, 1, 0, 0, 0, 1)))
            .await
            .unwrap();
        let VoiceInputEvent::CaptureFinish(finish) = recording.recv().await.unwrap().unwrap()
        else {
            panic!("finish barrier expected on the voice queue")
        };
        assert_eq!(
            *finish.boundary(),
            CaptureFinishBoundary::new(id, 1, false, 0, 0, 1).unwrap()
        );
        PendingFinishFixture {
            peer,
            task,
            recording,
            _recognition: recognition,
            io,
            finish,
        }
    }

    async fn assert_no_ack_and_failed_close(
        server: &MediaServer,
        fixture: PendingFinishFixture,
        cancellation: CancellationToken,
        expect_ok: bool,
    ) {
        let PendingFinishFixture {
            mut peer,
            task,
            recording,
            _recognition,
            io,
            finish,
        } = fixture;
        let result = tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, task)
            .await
            .expect("connection did not settle")
            .unwrap();
        assert_eq!(result.is_ok(), expect_ok, "{result:?}");
        let mut byte = [0_u8; 1];
        assert!(
            matches!(peer.read(&mut byte).await, Ok(0) | Err(_)),
            "a capture finish acknowledgement was written without recorder success"
        );
        // The single owned operation is gone: a late recorder completion is ignored.
        finish.complete(Ok(()));
        assert!(!server
            .inner
            .sessions
            .lock()
            .await
            .get(&41)
            .is_some_and(|session| session.active));
        drop(recording);
        cancellation.cancel();
        io.completion.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn drainable_finish_deadline_expires_without_ack_and_reaps_the_operation() {
        let server = timed_server().await;
        let cancellation = CancellationToken::new();
        let fixture = drainable_pending_finish(&server, &cancellation).await;
        tokio::time::advance(CAPTURE_FINISH_TIMEOUT - Duration::from_millis(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !fixture.task.is_finished(),
            "finish failed before its deadline"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_no_ack_and_failed_close(&server, fixture, cancellation, false).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn drainable_finish_cancellation_revocation_and_dropped_barrier_yield_no_ack() {
        for action in ["cancel", "revoke", "drop_barrier", "drop_voice"] {
            let server = timed_server().await;
            let cancellation = CancellationToken::new();
            let mut fixture = drainable_pending_finish(&server, &cancellation).await;
            match action {
                "cancel" => server.inner.cancellation.cancel(),
                "revoke" => server.revoke(SessionId::new(41)).await,
                "drop_barrier" => {
                    // The voice loop exited while holding the barrier.
                    let (replacement, _) = conversation_model_adapters::CaptureFinish::new(
                        fixture.finish.boundary().clone(),
                    );
                    drop(std::mem::replace(&mut fixture.finish, replacement));
                }
                _ => {
                    // Voice input closed (natural terminal) before the recorder ran.
                    let (_, closed) = tokio::sync::mpsc::channel(1);
                    drop(std::mem::replace(&mut fixture.recording, closed));
                    let (replacement, _) = conversation_model_adapters::CaptureFinish::new(
                        fixture.finish.boundary().clone(),
                    );
                    drop(std::mem::replace(&mut fixture.finish, replacement));
                }
            }
            assert_no_ack_and_failed_close(&server, fixture, cancellation, action == "cancel")
                .await;
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn drainable_finish_ack_write_failure_is_a_truthful_false_negative() {
        let server = timed_server().await;
        let cancellation = CancellationToken::new();
        let PendingFinishFixture {
            peer,
            task,
            recording,
            _recognition,
            io,
            finish,
        } = drainable_pending_finish(&server, &cancellation).await;
        // The peer is gone before the recorder confirms; the biased loop settles
        // the recorder result first and the acknowledgement write must fail.
        drop(peer);
        finish.complete(Ok(()));
        let result = tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "acknowledgement write failure was reported as success"
        );
        assert!(!server.inner.sessions.lock().await[&41].active);
        assert!(
            server.inner.sessions.lock().await[&41].capture_finished,
            "failed acknowledgement reopened the capture lane"
        );
        drop(recording);
        cancellation.cancel();
        io.completion.await.unwrap().unwrap();
        server.shutdown().await;
    }

    /// After kind 11 the producer may still write pre-close media-v1
    /// acknowledgements; planned completion is its clean EOF, not the ACK.
    #[tokio::test]
    async fn drainable_finish_planned_close_waits_for_the_producer_eof() {
        use tokio::sync::mpsc::error::TryRecvError;
        let server = timed_server().await;
        let cancellation = CancellationToken::new();
        let PendingFinishFixture {
            mut peer,
            task,
            mut recording,
            _recognition,
            io,
            finish,
        } = drainable_pending_finish(&server, &cancellation).await;
        let reconnect_deadline = server.inner.sessions.lock().await[&41].reconnect_deadline;
        finish.complete(Ok(()));
        let mut ack = [0_u8; 51];
        peer.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack[..10], b"MED3\x03\x0b\x00\x00\x00\x29");
        let mut late = b"MED1\x01\x03".to_vec();
        late.extend(41_u64.to_be_bytes());
        late.extend(1_u64.to_be_bytes());
        late.push(1);
        peer.write_all(&late)
            .await
            .expect("server closed before the producer finished");
        peer.flush().await.unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            peer.read(&mut byte).await.unwrap(),
            0,
            "no FIN after the ACK"
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!task.is_finished(), "server closed before the producer EOF");
        drop(peer);
        let result = tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_ok(),
            "clean producer EOF was not planned completion: {result:?}"
        );
        let sessions = server.inner.sessions.lock().await;
        assert!(!sessions[&41].active);
        assert_eq!(
            sessions[&41].reconnect_deadline, reconnect_deadline,
            "planned completion set a reconnect window"
        );
        drop(sessions);
        assert!(matches!(recording.try_recv(), Err(TryRecvError::Empty)));
        drop(recording);
        cancellation.cancel();
        io.completion.await.unwrap().unwrap();
        server.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn drainable_finish_acked_producer_that_never_closes_fails_at_the_deadline() {
        let server = timed_server().await;
        let cancellation = CancellationToken::new();
        let PendingFinishFixture {
            mut peer,
            task,
            recording,
            _recognition,
            io,
            finish,
        } = drainable_pending_finish(&server, &cancellation).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        finish.complete(Ok(()));
        let mut ack = [0_u8; 51];
        peer.read_exact(&mut ack).await.unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        tokio::time::advance(Duration::from_secs(6) - Duration::from_millis(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task.is_finished(),
            "acked connection failed before the deadline"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        let result = tokio::time::timeout(MEDIA_CONNECTION_REAP_TIMEOUT, task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "silent producer was treated as planned completion"
        );
        assert_eq!(
            peer.read(&mut byte).await.unwrap(),
            0,
            "a second packet followed the ACK"
        );
        assert!(!server.inner.sessions.lock().await[&41].active);
        drop(recording);
        cancellation.cancel();
        io.completion.await.unwrap().unwrap();
        server.shutdown().await;
    }
}
