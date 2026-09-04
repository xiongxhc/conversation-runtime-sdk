//! In-process half of the authenticated network media bridge.
//!
//! The gateway owns socket authentication; this module owns the bounded fan-out into the two
//! independently consumed runtime ports. Keeping the ports distinct prevents a recognizer change
//! from accidentally bypassing encrypted capture recording.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use conversation_protocol::{GenerationId, PlaybackState, SessionId, TurnId, UtteranceId};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioCapture, AudioFrame, CaptureEvent, ContinuousAudioOutput,
    PlaybackReceipt, VoiceCaptureControl, VoiceInput, VoiceInputEvent, VoiceIoFactory,
    VoiceIoSession,
};

const MEDIA_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPlaybackAck {
    Accepted,
    Rendered,
}

#[derive(Debug)]
pub struct NetworkPlaybackRequest {
    sequence: u64,
    frame: AudioFrame,
}

#[derive(Clone, Debug)]
pub struct NetworkFlushRequest {
    generation_id: GenerationId,
}

impl NetworkFlushRequest {
    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }
}

#[derive(Debug)]
pub enum NetworkBridgeRequest {
    Playback(NetworkPlaybackRequest),
    Flush(NetworkFlushRequest),
}

impl NetworkPlaybackRequest {
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn frame(&self) -> &AudioFrame {
        &self.frame
    }
}

#[derive(Clone)]
pub struct NetworkMediaHub {
    sessions: Arc<Mutex<HashMap<u64, NetworkSession>>>,
    max_playback_queue_bytes: usize,
    operation_timeout: Duration,
}

impl Default for NetworkMediaHub {
    fn default() -> Self {
        Self::with_max_playback_queue_bytes(MEDIA_QUEUE_CAPACITY * crate::MAX_PCM_FRAME_BYTES)
    }
}

struct NetworkSession {
    voice: Option<mpsc::Sender<Result<VoiceInputEvent, AdapterError>>>,
    recognition: Option<mpsc::Sender<Result<CaptureEvent, AdapterError>>>,
    playback: Option<NetworkPlaybackTransport>,
    next_playback_sequence: u64,
    pending_playback: HashMap<u64, PendingPlayback>,
    pending_flush: HashMap<u64, PendingFlush>,
    reconnect_deadline: Option<Instant>,
    waiting_playback: VecDeque<WaitingPlayback>,
    waiting_playback_bytes: usize,
}

impl Default for NetworkSession {
    fn default() -> Self {
        Self {
            voice: None,
            recognition: None,
            playback: None,
            next_playback_sequence: 1,
            pending_playback: HashMap::new(),
            pending_flush: HashMap::new(),
            reconnect_deadline: None,
            waiting_playback: VecDeque::new(),
            waiting_playback_bytes: 0,
        }
    }
}

#[derive(Clone)]
struct NetworkPlaybackTransport {
    connection_id: u64,
    sender: mpsc::Sender<NetworkBridgeRequest>,
}

struct PendingPlayback {
    connection_id: Option<u64>,
    generation_id: GenerationId,
    completion: Option<oneshot::Sender<Result<PlaybackReceipt, AdapterError>>>,
    accepted: bool,
    written: bool,
}

struct WaitingPlayback {
    sequence: u64,
    frame: AudioFrame,
}

struct PendingFlush {
    connection_id: u64,
    completion: oneshot::Sender<Result<PlaybackReceipt, AdapterError>>,
}

impl NetworkMediaHub {
    pub fn with_max_playback_queue_bytes(max_playback_queue_bytes: usize) -> Self {
        Self::with_limits(max_playback_queue_bytes, Duration::from_secs(10))
    }

    pub fn with_limits(max_playback_queue_bytes: usize, operation_timeout: Duration) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            max_playback_queue_bytes,
            operation_timeout,
        }
    }

    pub async fn reserve(&self, session_id: SessionId) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id.get()) {
            return Err(AdapterError::new(
                "network media session is already reserved",
            ));
        }
        sessions.insert(session_id.get(), NetworkSession::default());
        Ok(())
    }

    pub async fn revoke(&self, session_id: SessionId) {
        self.sessions.lock().await.remove(&session_id.get());
    }

    async fn attach_voice(
        &self,
        session_id: SessionId,
    ) -> Result<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>, AdapterError> {
        let (sender, receiver) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
        if session.voice.replace(sender).is_some() {
            return Err(AdapterError::new(
                "network media voice input is already attached",
            ));
        }
        Ok(receiver)
    }

    async fn attach_recognition(
        &self,
        session_id: SessionId,
    ) -> Result<mpsc::Receiver<Result<CaptureEvent, AdapterError>>, AdapterError> {
        let (sender, receiver) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
        if session.recognition.replace(sender).is_some() {
            return Err(AdapterError::new(
                "network media recognition input is already attached",
            ));
        }
        Ok(receiver)
    }

    /// Delivers one validated bridge capture frame to both consumers. A missing consumer is a
    /// failed-closed condition: the socket layer must not call this an accepted media frame.
    pub async fn deliver_capture(
        &self,
        session_id: SessionId,
        frame: AudioFrame,
    ) -> Result<(), AdapterError> {
        let (voice, recognition) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            (
                session.voice.clone().ok_or_else(|| {
                    AdapterError::new("network media voice input is not attached")
                })?,
                session.recognition.clone().ok_or_else(|| {
                    AdapterError::new("network media recognition input is not attached")
                })?,
            )
        };
        let (voice_permit, recognition_permit) =
            tokio::time::timeout(self.operation_timeout, async {
                tokio::try_join!(voice.reserve(), recognition.reserve())
            })
            .await
            .map_err(|_| AdapterError::new("network media capture fan-out timed out"))?
            .map_err(|_| AdapterError::new("network media capture input closed"))?;
        voice_permit.send(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(
            frame.clone(),
        ))));
        recognition_permit.send(Ok(CaptureEvent::Frame(frame)));
        Ok(())
    }

    pub async fn deliver_discontinuity(
        &self,
        session_id: SessionId,
        at_ms: u64,
    ) -> Result<(), AdapterError> {
        let (voice, recognition) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            (
                session.voice.clone().ok_or_else(|| {
                    AdapterError::new("network media voice input is not attached")
                })?,
                session.recognition.clone().ok_or_else(|| {
                    AdapterError::new("network media recognition input is not attached")
                })?,
            )
        };
        let (voice_permit, recognition_permit) =
            tokio::time::timeout(self.operation_timeout, async {
                tokio::try_join!(voice.reserve(), recognition.reserve())
            })
            .await
            .map_err(|_| AdapterError::new("network media discontinuity fan-out timed out"))?
            .map_err(|_| AdapterError::new("network media capture input closed"))?;
        voice_permit.send(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
            at_ms,
        })));
        recognition_permit.send(Ok(CaptureEvent::Discontinuity { at_ms }));
        Ok(())
    }

    pub async fn attach_playback_transport(
        &self,
        session_id: SessionId,
        connection_id: u64,
    ) -> Result<mpsc::Receiver<NetworkBridgeRequest>, AdapterError> {
        let (sender, receiver) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
        if session.playback.is_some() {
            return Err(AdapterError::new(
                "network media playback transport is already attached",
            ));
        }
        session.playback = Some(NetworkPlaybackTransport {
            connection_id,
            sender: sender.clone(),
        });
        session.reconnect_deadline = None;
        while let Some(waiting) = session.waiting_playback.pop_front() {
            session.waiting_playback_bytes = session
                .waiting_playback_bytes
                .saturating_sub(waiting.frame.bytes().len());
            let pending = session
                .pending_playback
                .get_mut(&waiting.sequence)
                .expect("waiting playback remains pending");
            pending.connection_id = Some(connection_id);
            sender
                .try_send(NetworkBridgeRequest::Playback(NetworkPlaybackRequest {
                    sequence: waiting.sequence,
                    frame: waiting.frame,
                }))
                .map_err(|_| AdapterError::new("network media playback queue is unavailable"))?;
        }
        Ok(receiver)
    }

    pub async fn detach_playback_transport(&self, session_id: SessionId, connection_id: u64) {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            if session
                .playback
                .as_ref()
                .is_some_and(|transport| transport.connection_id == connection_id)
            {
                session.playback = None;
            }
        }
    }

    pub async fn mark_playback_written(
        &self,
        session_id: SessionId,
        connection_id: u64,
        sequence: u64,
    ) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
        let pending = session
            .pending_playback
            .get_mut(&sequence)
            .ok_or_else(|| AdapterError::new("network media playback write was unknown"))?;
        if pending.connection_id != Some(connection_id) {
            return Err(AdapterError::new(
                "network media playback write connection did not match",
            ));
        }
        pending.written = true;
        Ok(())
    }

    pub async fn disconnect_playback_transport(
        &self,
        session_id: SessionId,
        connection_id: u64,
        reconnect_deadline: Instant,
    ) -> Result<(), AdapterError> {
        let (voice, completions, flushed_generations) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if session
                .playback
                .as_ref()
                .is_none_or(|transport| transport.connection_id != connection_id)
            {
                return Ok(());
            }
            session.playback = None;
            session.reconnect_deadline = Some(reconnect_deadline);
            let sequences: Vec<_> = session
                .pending_playback
                .iter()
                .filter_map(|(sequence, pending)| {
                    (pending.connection_id == Some(connection_id)).then_some(*sequence)
                })
                .collect();
            let mut completions = Vec::new();
            let mut flushed_generations = HashSet::new();
            for sequence in sequences {
                if let Some(mut pending) = session.pending_playback.remove(&sequence) {
                    if let Some(completion) = pending.completion.take() {
                        completions.push(completion);
                    }
                    if pending.written {
                        flushed_generations.insert(pending.generation_id);
                    }
                }
            }
            let generations: Vec<_> = session
                .pending_flush
                .iter()
                .filter_map(|(generation, pending)| {
                    (pending.connection_id == connection_id).then_some(*generation)
                })
                .collect();
            for generation in generations {
                if let Some(pending) = session.pending_flush.remove(&generation) {
                    completions.push(pending.completion);
                }
            }
            (
                session.voice.clone(),
                completions,
                flushed_generations.into_iter().collect::<Vec<_>>(),
            )
        };
        for completion in completions {
            let _ = completion.send(Err(AdapterError::new(
                "network media bridge disconnected before acknowledgement",
            )));
        }
        if !flushed_generations.is_empty() {
            let voice = voice
                .ok_or_else(|| AdapterError::new("network media voice input is not attached"))?;
            for generation_id in flushed_generations {
                tokio::time::timeout(
                    self.operation_timeout,
                    voice.send(Ok(VoiceInputEvent::Playback(PlaybackReceipt::new(
                        generation_id,
                        PlaybackState::Flushed,
                    )))),
                )
                .await
                .map_err(|_| AdapterError::new("network media playback event timed out"))?
                .map_err(|_| AdapterError::new("network media voice input closed"))?;
            }
        }
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(reconnect_deadline)).await;
            hub.expire_waiting_playback(session_id, reconnect_deadline)
                .await;
        });
        Ok(())
    }

    async fn expire_waiting_playback(&self, session_id: SessionId, reconnect_deadline: Instant) {
        let completions = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(&session_id.get()) else {
                return;
            };
            if session.reconnect_deadline != Some(reconnect_deadline) {
                return;
            }
            session.reconnect_deadline = None;
            session.waiting_playback_bytes = 0;
            let sequences: Vec<_> = session
                .waiting_playback
                .drain(..)
                .map(|waiting| waiting.sequence)
                .collect();
            sequences
                .into_iter()
                .filter_map(|sequence| session.pending_playback.remove(&sequence))
                .filter_map(|mut pending| pending.completion.take())
                .collect::<Vec<_>>()
        };
        for completion in completions {
            let _ = completion.send(Err(AdapterError::new(
                "network media playback reconnect window expired",
            )));
        }
    }

    pub async fn enqueue_playback(
        &self,
        session_id: SessionId,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> Result<PlaybackReceipt, AdapterError> {
        if cancellation.is_cancelled() {
            return Err(AdapterError::new("network media playback was cancelled"));
        }
        let (sequence, sender, completion) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if session.pending_playback.len() >= MEDIA_QUEUE_CAPACITY {
                return Err(AdapterError::new(
                    "network media playback queue exceeded its configured bound",
                ));
            }
            let now = Instant::now();
            let transport = session.playback.as_ref();
            if transport.is_none()
                && session
                    .reconnect_deadline
                    .is_none_or(|deadline| now > deadline)
            {
                return Err(AdapterError::new(
                    "network media bridge is not connected within its reconnect window",
                ));
            }
            if transport.is_none()
                && (frame.bytes().len() > self.max_playback_queue_bytes
                    || session
                        .waiting_playback_bytes
                        .saturating_add(frame.bytes().len())
                        > self.max_playback_queue_bytes)
            {
                return Err(AdapterError::new(
                    "network media playback queue exceeded its byte bound",
                ));
            }
            let sequence = session.next_playback_sequence;
            session.next_playback_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| AdapterError::new("network media playback sequence overflowed"))?;
            let (completion_sender, completion) = oneshot::channel();
            session.pending_playback.insert(
                sequence,
                PendingPlayback {
                    connection_id: transport.map(|transport| transport.connection_id),
                    generation_id: frame.generation_id(),
                    completion: Some(completion_sender),
                    accepted: false,
                    written: false,
                },
            );
            if let Some(transport) = transport {
                (sequence, Some(transport.sender.clone()), completion)
            } else {
                session.waiting_playback_bytes = session
                    .waiting_playback_bytes
                    .saturating_add(frame.bytes().len());
                session.waiting_playback.push_back(WaitingPlayback {
                    sequence,
                    frame: frame.clone(),
                });
                (sequence, None, completion)
            }
        };
        if let Some(sender) = sender {
            if sender
                .try_send(NetworkBridgeRequest::Playback(NetworkPlaybackRequest {
                    sequence,
                    frame,
                }))
                .is_err()
            {
                self.cancel_pending_playback(session_id, sequence).await;
                return Err(AdapterError::new(
                    "network media playback queue is unavailable",
                ));
            }
        }
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AdapterError::new("network media playback was cancelled")),
            _ = tokio::time::sleep(self.operation_timeout) => Err(AdapterError::new("network media playback acknowledgement timed out")),
            result = completion => result.unwrap_or_else(|_| Err(AdapterError::new("network media playback acknowledgement closed"))),
        };
        if result.is_err() {
            self.cancel_pending_playback(session_id, sequence).await;
        }
        result
    }

    pub async fn acknowledge_playback(
        &self,
        session_id: SessionId,
        connection_id: u64,
        sequence: u64,
        acknowledgement: NetworkPlaybackAck,
    ) -> Result<(), AdapterError> {
        let (accepted_completion, rendered, accepted_expiry) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if session
                .playback
                .as_ref()
                .is_none_or(|transport| transport.connection_id != connection_id)
            {
                return Err(AdapterError::new(
                    "network media playback acknowledgement connection did not match",
                ));
            }
            let pending = session.pending_playback.get_mut(&sequence).ok_or_else(|| {
                AdapterError::new("network media playback acknowledgement was unknown")
            })?;
            if pending.connection_id != Some(connection_id) {
                return Err(AdapterError::new(
                    "network media playback acknowledgement connection did not match",
                ));
            }
            match acknowledgement {
                NetworkPlaybackAck::Accepted if pending.accepted => {
                    return Err(AdapterError::new(
                        "network media playback acknowledgement was duplicated",
                    ));
                }
                NetworkPlaybackAck::Accepted => {
                    pending.accepted = true;
                    let receipt =
                        PlaybackReceipt::new(pending.generation_id, PlaybackState::Accepted);
                    (
                        pending
                            .completion
                            .take()
                            .map(|completion| (completion, receipt)),
                        None,
                        Some((session_id, connection_id, sequence)),
                    )
                }
                NetworkPlaybackAck::Rendered if !pending.accepted => {
                    return Err(AdapterError::new(
                        "network media playback rendered before acceptance",
                    ));
                }
                NetworkPlaybackAck::Rendered => {
                    let pending = session
                        .pending_playback
                        .remove(&sequence)
                        .expect("pending playback acknowledgement remains registered");
                    let voice = session.voice.clone().ok_or_else(|| {
                        AdapterError::new("network media voice input is not attached")
                    })?;
                    (
                        None,
                        Some((
                            voice,
                            PlaybackReceipt::new(pending.generation_id, PlaybackState::Rendered),
                        )),
                        None,
                    )
                }
            }
        };
        if let Some((completion, receipt)) = accepted_completion {
            let _ = completion.send(Ok(receipt));
        }
        if let Some((voice, receipt)) = rendered {
            tokio::time::timeout(
                self.operation_timeout,
                voice.send(Ok(VoiceInputEvent::Playback(receipt))),
            )
            .await
            .map_err(|_| AdapterError::new("network media playback event timed out"))?
            .map_err(|_| AdapterError::new("network media voice input closed"))?;
        }
        if let Some((session_id, connection_id, sequence)) = accepted_expiry {
            let hub = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(hub.operation_timeout).await;
                let _ = hub
                    .expire_accepted_playback(session_id, connection_id, sequence)
                    .await;
            });
        }
        Ok(())
    }

    async fn expire_accepted_playback(
        &self,
        session_id: SessionId,
        connection_id: u64,
        sequence: u64,
    ) -> Result<(), AdapterError> {
        let (voice, receipt) = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(&session_id.get()) else {
                return Ok(());
            };
            if session
                .playback
                .as_ref()
                .is_none_or(|transport| transport.connection_id != connection_id)
            {
                return Ok(());
            }
            let Some(pending) = session.pending_playback.get(&sequence) else {
                return Ok(());
            };
            if !pending.accepted || pending.connection_id != Some(connection_id) {
                return Ok(());
            }
            let pending = session
                .pending_playback
                .remove(&sequence)
                .expect("accepted playback remains pending until expiry");
            (
                session.voice.clone(),
                PlaybackReceipt::new(pending.generation_id, PlaybackState::Flushed),
            )
        };
        if let Some(voice) = voice {
            tokio::time::timeout(
                self.operation_timeout,
                voice.send(Ok(VoiceInputEvent::Playback(receipt))),
            )
            .await
            .map_err(|_| AdapterError::new("network media playback expiry event timed out"))?
            .map_err(|_| AdapterError::new("network media voice input closed"))?;
        }
        Ok(())
    }

    pub async fn request_flush(
        &self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> Result<PlaybackReceipt, AdapterError> {
        let (sender, completion) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            let transport = session
                .playback
                .as_ref()
                .ok_or_else(|| AdapterError::new("network media bridge is not connected"))?;
            if session.pending_flush.len() >= MEDIA_QUEUE_CAPACITY {
                return Err(AdapterError::new(
                    "network media flush queue exceeded its configured bound",
                ));
            }
            if session.pending_flush.contains_key(&generation_id.get()) {
                return Err(AdapterError::new("network media flush was already pending"));
            }
            let (completion_sender, completion) = oneshot::channel();
            session.pending_flush.insert(
                generation_id.get(),
                PendingFlush {
                    connection_id: transport.connection_id,
                    completion: completion_sender,
                },
            );
            (transport.sender.clone(), completion)
        };
        if sender
            .try_send(NetworkBridgeRequest::Flush(NetworkFlushRequest {
                generation_id,
            }))
            .is_err()
        {
            self.cancel_pending_flush(session_id, generation_id).await;
            return Err(AdapterError::new(
                "network media flush queue is unavailable",
            ));
        }
        match tokio::time::timeout(self.operation_timeout, completion).await {
            Ok(result) => result.unwrap_or_else(|_| {
                Err(AdapterError::new(
                    "network media flush acknowledgement closed",
                ))
            }),
            Err(_) => {
                self.cancel_pending_flush(session_id, generation_id).await;
                Err(AdapterError::new(
                    "network media flush acknowledgement timed out",
                ))
            }
        }
    }

    pub async fn acknowledge_flush(
        &self,
        session_id: SessionId,
        connection_id: u64,
        generation_id: GenerationId,
    ) -> Result<(), AdapterError> {
        let completion = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if session
                .playback
                .as_ref()
                .is_none_or(|transport| transport.connection_id != connection_id)
            {
                return Err(AdapterError::new(
                    "network media flush acknowledgement connection did not match",
                ));
            }
            let pending = session
                .pending_flush
                .remove(&generation_id.get())
                .ok_or_else(|| {
                    AdapterError::new("network media flush acknowledgement was unknown")
                })?;
            if pending.connection_id != connection_id {
                return Err(AdapterError::new(
                    "network media flush acknowledgement connection did not match",
                ));
            }
            pending.completion
        };
        let _ = completion.send(Ok(PlaybackReceipt::new(
            generation_id,
            PlaybackState::Flushed,
        )));
        Ok(())
    }

    async fn cancel_pending_playback(&self, session_id: SessionId, sequence: u64) {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            session.pending_playback.remove(&sequence);
            if let Some(index) = session
                .waiting_playback
                .iter()
                .position(|waiting| waiting.sequence == sequence)
            {
                let waiting = session
                    .waiting_playback
                    .remove(index)
                    .expect("waiting playback index remains valid");
                session.waiting_playback_bytes = session
                    .waiting_playback_bytes
                    .saturating_sub(waiting.frame.bytes().len());
            }
        }
    }

    async fn cancel_pending_flush(&self, session_id: SessionId, generation_id: GenerationId) {
        if let Some(session) = self.sessions.lock().await.get_mut(&session_id.get()) {
            session.pending_flush.remove(&generation_id.get());
        }
    }
}

#[derive(Clone)]
pub struct NetworkVoiceIoFactory {
    hub: NetworkMediaHub,
    session_id: SessionId,
}

impl NetworkVoiceIoFactory {
    pub fn new(hub: NetworkMediaHub, session_id: SessionId) -> Self {
        Self { hub, session_id }
    }

    pub fn recognition_capture(&self) -> Arc<dyn AudioCapture> {
        Arc::new(NetworkRecognitionCapture {
            hub: self.hub.clone(),
            session_id: self.session_id,
            receiver: Mutex::new(None),
        })
    }

    /// Attaches the recognizer branch before a lease is disclosed. The returned capture is
    /// retained by the gateway until the later provider wiring takes ownership of this exact
    /// bounded receiver; it must not be recreated after bridge ingress begins.
    pub async fn attach_recognition_capture(&self) -> Result<Arc<dyn AudioCapture>, AdapterError> {
        let receiver = self.hub.attach_recognition(self.session_id).await?;
        Ok(Arc::new(NetworkRecognitionCapture {
            hub: self.hub.clone(),
            session_id: self.session_id,
            receiver: Mutex::new(Some(receiver)),
        }))
    }
}

impl VoiceIoFactory for NetworkVoiceIoFactory {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new(
                    "network media session identity did not match",
                ));
            }
            Ok(VoiceIoSession {
                input: Arc::new(NetworkVoiceInput {
                    hub: self.hub.clone(),
                    session_id,
                }),
                capture: Arc::new(NetworkCaptureControl),
                output: Arc::new(NetworkAudioOutput {
                    hub: self.hub.clone(),
                    session_id,
                }),
                completion: tokio::spawn(async move {
                    cancellation.cancelled().await;
                    Ok(())
                }),
            })
        })
    }
}

struct NetworkVoiceInput {
    hub: NetworkMediaHub,
    session_id: SessionId,
}

impl VoiceInput for NetworkVoiceInput {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new(
                    "network media session identity did not match",
                ));
            }
            self.hub.attach_voice(session_id).await
        })
    }
}

struct NetworkRecognitionCapture {
    hub: NetworkMediaHub,
    session_id: SessionId,
    receiver: Mutex<Option<mpsc::Receiver<Result<CaptureEvent, AdapterError>>>>,
}

impl AudioCapture for NetworkRecognitionCapture {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new(
                    "network media session identity did not match",
                ));
            }
            let mut receiver = self.receiver.lock().await;
            match receiver.take() {
                Some(receiver) => Ok(receiver),
                None => {
                    drop(receiver);
                    self.hub.attach_recognition(session_id).await
                }
            }
        })
    }
}

struct NetworkCaptureControl;

impl VoiceCaptureControl for NetworkCaptureControl {
    fn pause<'a>(
        &'a self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn resume<'a>(
        &'a self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct NetworkAudioOutput {
    hub: NetworkMediaHub,
    session_id: SessionId,
}

impl ContinuousAudioOutput for NetworkAudioOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.hub
                .enqueue_playback(self.session_id, frame, cancellation)
                .await
        })
    }

    fn flush<'a>(
        &'a self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            if session_id != self.session_id {
                return Err(AdapterError::new(
                    "network media session identity did not match",
                ));
            }
            self.hub.request_flush(session_id, generation_id).await
        })
    }
}

pub fn frame_for_network_capture(
    sequence: u64,
    format: crate::PcmFormat,
    bytes: Vec<u8>,
) -> Result<AudioFrame, AdapterError> {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        format,
        bytes,
    )
}
