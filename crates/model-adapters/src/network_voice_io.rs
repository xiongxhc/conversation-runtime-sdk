//! In-process half of the authenticated network media bridge.
//!
//! The gateway owns socket authentication; this module owns the bounded fan-out into the two
//! independently consumed runtime ports. Keeping the ports distinct prevents a recognizer change
//! from accidentally bypassing encrypted capture recording.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use conversation_protocol::{GenerationId, PlaybackState, SessionId, TurnId, UtteranceId};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioCapture, AudioFrame, CaptureEvent, CaptureFinish,
    CaptureFinishBoundary, CaptureMetadata, ContinuousAudioOutput, PlaybackReceipt,
    VoiceCaptureControl, VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
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
    shutdown: CancellationToken,
}

impl Default for NetworkMediaHub {
    fn default() -> Self {
        Self::with_max_playback_queue_bytes(MEDIA_QUEUE_CAPACITY * crate::MAX_PCM_FRAME_BYTES)
    }
}

struct NetworkSession {
    reservation_epoch: u64,
    expiry_cancellation: CancellationToken,
    expiry_changed: Arc<Notify>,
    expiry_task: Option<JoinHandle<()>>,
    voice: Option<mpsc::Sender<Result<VoiceInputEvent, AdapterError>>>,
    recognition: Option<RecognitionLane>,
    playback: Option<NetworkPlaybackTransport>,
    next_playback_sequence: u64,
    pending_playback: HashMap<u64, PendingPlayback>,
    pending_flush: HashMap<u64, PendingFlush>,
    reconnect_deadline: Option<Instant>,
    playback_finished: bool,
    finish_connection_id: Option<u64>,
    finish_delivered: bool,
    waiting_playback: VecDeque<WaitingPlayback>,
    waiting_playback_bytes: usize,
}

impl NetworkSession {
    fn new(
        reservation_epoch: u64,
        expiry_cancellation: CancellationToken,
        expiry_changed: Arc<Notify>,
    ) -> Self {
        Self {
            reservation_epoch,
            expiry_cancellation,
            expiry_changed,
            expiry_task: None,
            voice: None,
            recognition: None,
            playback: None,
            next_playback_sequence: 1,
            pending_playback: HashMap::new(),
            pending_flush: HashMap::new(),
            reconnect_deadline: None,
            playback_finished: false,
            finish_connection_id: None,
            finish_delivered: false,
            waiting_playback: VecDeque::new(),
            waiting_playback_bytes: 0,
        }
    }
}

/// Recognition is a lossy branch of capture that never waits. When its bounded queue is full the
/// event is dropped and one discontinuity is owed; it is delivered, in order, before the next
/// recognition event that fits. The owed gap travels with the retained sink. A closed receiver
/// still fails capture closed before recording is touched (the gateway keeps it open across ASR
/// failure for the media lifetime).
#[derive(Clone)]
struct RecognitionLane {
    sender: mpsc::Sender<Result<CaptureEvent, AdapterError>>,
    owed_gap_ms: Arc<std::sync::Mutex<Option<u64>>>,
}

impl RecognitionLane {
    fn ensure_open(&self) -> Result<(), AdapterError> {
        if self.sender.is_closed() {
            return Err(AdapterError::new("network media capture input closed"));
        }
        Ok(())
    }

    fn offer(&self, event: CaptureEvent) {
        // Held only across non-blocking sends, never across an await.
        let mut owed = self
            .owed_gap_ms
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match (*owed, event) {
            (None, event) => {
                let lost_at_ms = match &event {
                    CaptureEvent::Discontinuity { at_ms } => *at_ms,
                    CaptureEvent::Frame(_) => 0,
                };
                if self.sender.try_send(Ok(event)).is_err() {
                    *owed = Some(lost_at_ms);
                }
            }
            (Some(at_ms), CaptureEvent::Discontinuity { .. }) => {
                if self
                    .sender
                    .try_send(Ok(CaptureEvent::Discontinuity { at_ms }))
                    .is_ok()
                {
                    *owed = None;
                }
            }
            (Some(at_ms), event) => {
                if let Ok(mut permits) = self.sender.try_reserve_many(2) {
                    if let (Some(gap), Some(next)) = (permits.next(), permits.next()) {
                        gap.send(Ok(CaptureEvent::Discontinuity { at_ms }));
                        next.send(Ok(event));
                        *owed = None;
                    }
                }
            }
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
    accepted_deadline: Option<Instant>,
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
            shutdown: CancellationToken::new(),
        }
    }

    pub async fn reserve(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
    ) -> Result<(), AdapterError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id.get()) {
            return Err(AdapterError::new(
                "network media session is already reserved",
            ));
        }
        let expiry_cancellation = CancellationToken::new();
        let expiry_changed = Arc::new(Notify::new());
        let worker = tokio::spawn(accepted_expiry_worker(
            self.clone(),
            session_id,
            reservation_epoch,
            expiry_cancellation.clone(),
            self.shutdown.clone(),
            Arc::clone(&expiry_changed),
        ));
        let mut session =
            NetworkSession::new(reservation_epoch, expiry_cancellation, expiry_changed);
        session.expiry_task = Some(worker);
        sessions.insert(session_id.get(), session);
        Ok(())
    }

    pub async fn revoke(&self, session_id: SessionId) {
        let task = self
            .sessions
            .lock()
            .await
            .remove(&session_id.get())
            .and_then(|mut session| {
                session.expiry_cancellation.cancel();
                session.expiry_changed.notify_waiters();
                session.expiry_task.take()
            });
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Wakes reservation-scoped expiry workers synchronously so dropping the server cannot leave
    /// a self-retaining timer behind. `shutdown` awaits their reaping after transports exit.
    pub fn cancel_expiry_workers(&self) {
        self.shutdown.cancel();
    }

    pub async fn shutdown(&self) {
        self.cancel_expiry_workers();
        let tasks = self
            .sessions
            .lock()
            .await
            .drain()
            .filter_map(|(_, mut session)| {
                session.expiry_cancellation.cancel();
                session.expiry_changed.notify_waiters();
                session.expiry_task.take()
            })
            .collect::<Vec<_>>();
        for task in tasks {
            let _ = task.await;
        }
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
        let lane = RecognitionLane {
            sender,
            owed_gap_ms: Arc::new(std::sync::Mutex::new(None)),
        };
        if session.recognition.replace(lane).is_some() {
            return Err(AdapterError::new(
                "network media recognition input is already attached",
            ));
        }
        Ok(receiver)
    }

    /// Delivers one validated bridge capture frame to both consumers. A missing consumer is a
    /// failed-closed condition: the socket layer must not call this an accepted media frame.
    /// Only recording is awaited; recognition is offered afterwards and never delays it.
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
        recognition.ensure_open()?;
        let voice_permit = tokio::time::timeout(self.operation_timeout, voice.reserve())
            .await
            .map_err(|_| AdapterError::new("network media capture fan-out timed out"))?
            .map_err(|_| AdapterError::new("network media capture input closed"))?;
        voice_permit.send(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(
            frame.clone(),
        ))));
        recognition.offer(CaptureEvent::Frame(frame));
        Ok(())
    }

    /// Old connection cleanup selects only its reservation's sinks under the
    /// hub lock. Subsequent bounded sends retain those sinks across revoke/reuse.
    pub async fn deliver_disconnect_for_reservation(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        at_ms: u64,
        capture_epoch: Option<u64>,
    ) -> Result<(), AdapterError> {
        let (voice, recognition) = {
            let sessions = self.sessions.lock().await;
            let Some(session) = sessions
                .get(&session_id.get())
                .filter(|session| session.reservation_epoch == reservation_epoch)
            else {
                return Ok(());
            };
            (
                session.voice.clone().ok_or_else(|| {
                    AdapterError::new("network media voice input is not attached")
                })?,
                session.recognition.clone().ok_or_else(|| {
                    AdapterError::new("network media recognition input is not attached")
                })?,
            )
        };
        self.send_discontinuity(voice.clone(), recognition, at_ms)
            .await?;
        if let Some(capture_epoch) = capture_epoch {
            self.send_capture_metadata(
                voice,
                CaptureMetadata::Discontinuity {
                    capture_epoch,
                    reason: crate::CaptureDiscontinuityReason::Reconnect,
                },
            )
            .await?;
        }
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
        self.send_discontinuity(voice, recognition, at_ms).await
    }

    async fn send_discontinuity(
        &self,
        voice: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
        recognition: RecognitionLane,
        at_ms: u64,
    ) -> Result<(), AdapterError> {
        recognition.ensure_open()?;
        let voice_permit = tokio::time::timeout(self.operation_timeout, voice.reserve())
            .await
            .map_err(|_| AdapterError::new("network media discontinuity fan-out timed out"))?
            .map_err(|_| AdapterError::new("network media capture input closed"))?;
        voice_permit.send(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
            at_ms,
        })));
        recognition.offer(CaptureEvent::Discontinuity { at_ms });
        Ok(())
    }

    /// Bounded recorder-only evidence. The recognizer's channel and activity
    /// stream never receive epoch/clock records.
    pub async fn deliver_capture_metadata(
        &self,
        session_id: SessionId,
        metadata: CaptureMetadata,
    ) -> Result<(), AdapterError> {
        let voice = self
            .sessions
            .lock()
            .await
            .get(&session_id.get())
            .and_then(|session| session.voice.clone())
            .ok_or_else(|| AdapterError::new("network media voice input is not attached"))?;
        self.send_capture_metadata(voice, metadata).await
    }

    async fn send_capture_metadata(
        &self,
        voice: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
        metadata: CaptureMetadata,
    ) -> Result<(), AdapterError> {
        tokio::time::timeout(
            self.operation_timeout,
            voice.send(Ok(VoiceInputEvent::CaptureMetadata(metadata))),
        )
        .await
        .map_err(|_| AdapterError::new("network media metadata delivery timed out"))?
        .map_err(|_| AdapterError::new("network media capture input closed"))
    }

    /// Delivers one reservation-bound recorder barrier without involving recognition.
    pub async fn deliver_capture_finish_for_reservation(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        connection_id: u64,
        boundary: CaptureFinishBoundary,
    ) -> Result<oneshot::Receiver<Result<(), AdapterError>>, AdapterError> {
        if boundary.session_id() != session_id {
            return Err(AdapterError::new(
                "network media finish session did not match",
            ));
        }
        let voice = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .filter(|session| {
                    session.reservation_epoch == reservation_epoch
                        && session.playback_finished
                        && session.finish_connection_id == Some(connection_id)
                        && !session.finish_delivered
                })
                .ok_or_else(|| AdapterError::new("network media finish ownership did not match"))?;
            session.finish_delivered = true;
            session
                .voice
                .clone()
                .ok_or_else(|| AdapterError::new("network media voice input is not attached"))?
        };
        let (finish, completion) = CaptureFinish::new(boundary);
        tokio::time::timeout(
            self.operation_timeout,
            voice.send(Ok(VoiceInputEvent::CaptureFinish(finish))),
        )
        .await
        .map_err(|_| AdapterError::new("network media finish delivery timed out"))?
        .map_err(|_| AdapterError::new("network media capture input closed"))?;
        Ok(completion)
    }

    pub async fn attach_playback_transport(
        &self,
        session_id: SessionId,
        connection_id: u64,
    ) -> Result<mpsc::Receiver<NetworkBridgeRequest>, AdapterError> {
        self.attach_playback_transport_checked(session_id, None, connection_id)
            .await
    }

    /// An authentication continuation may outlive revoke/re-reserve. Check its
    /// reservation before installing a transport or moving queued playback.
    pub async fn attach_playback_transport_for_reservation(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        connection_id: u64,
    ) -> Result<mpsc::Receiver<NetworkBridgeRequest>, AdapterError> {
        self.attach_playback_transport_checked(session_id, Some(reservation_epoch), connection_id)
            .await
    }

    async fn attach_playback_transport_checked(
        &self,
        session_id: SessionId,
        reservation_epoch: Option<u64>,
        connection_id: u64,
    ) -> Result<mpsc::Receiver<NetworkBridgeRequest>, AdapterError> {
        let (sender, receiver) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id.get())
            .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
        if reservation_epoch.is_some_and(|epoch| epoch != session.reservation_epoch) {
            return Err(AdapterError::new(
                "network media reservation no longer matches",
            ));
        }
        if session.playback_finished {
            return Err(AdapterError::new("network media playback is finishing"));
        }
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
        session.expiry_changed.notify_one();
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

    /// Permanently detaches the exact finishing connection and settles its bounded owners.
    pub async fn planned_detach_playback_transport_for_finish(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        connection_id: u64,
    ) -> Result<(), AdapterError> {
        let (voice, completions, mut flushed_generations) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if session.reservation_epoch != reservation_epoch {
                return Err(AdapterError::new(
                    "network media finish reservation no longer matches",
                ));
            }
            if session.playback_finished
                || session
                    .playback
                    .as_ref()
                    .is_none_or(|transport| transport.connection_id != connection_id)
            {
                return Err(AdapterError::new(
                    "network media finish connection no longer matches",
                ));
            }
            if !session.waiting_playback.is_empty()
                || session.waiting_playback_bytes != 0
                || session
                    .pending_playback
                    .values()
                    .any(|pending| pending.connection_id != Some(connection_id))
                || session
                    .pending_flush
                    .values()
                    .any(|pending| pending.connection_id != connection_id)
            {
                return Err(AdapterError::new(
                    "network media finish found playback owned elsewhere",
                ));
            }
            session.playback = None;
            session.reconnect_deadline = None;
            session.playback_finished = true;
            session.finish_connection_id = Some(connection_id);
            session.finish_delivered = false;
            let mut completions = Vec::new();
            let mut flushed_generations = HashSet::new();
            for (_, mut pending) in session.pending_playback.drain() {
                if let Some(completion) = pending.completion.take() {
                    completions.push(completion);
                }
                if pending.written {
                    flushed_generations.insert(pending.generation_id);
                }
            }
            for (_, pending) in session.pending_flush.drain() {
                completions.push(pending.completion);
            }
            (
                session.voice.clone(),
                completions,
                flushed_generations.into_iter().collect::<Vec<_>>(),
            )
        };
        for completion in completions {
            let _ = completion.send(Err(AdapterError::new(
                "network media playback stopped for capture finish",
            )));
        }
        flushed_generations.sort_unstable_by_key(|generation| generation.get());
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
        Ok(())
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
        self.disconnect_playback_transport_checked(
            session_id,
            None,
            connection_id,
            reconnect_deadline,
        )
        .await
    }

    pub async fn disconnect_playback_transport_for_reservation(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        connection_id: u64,
        reconnect_deadline: Instant,
    ) -> Result<(), AdapterError> {
        self.disconnect_playback_transport_checked(
            session_id,
            Some(reservation_epoch),
            connection_id,
            reconnect_deadline,
        )
        .await
    }

    async fn disconnect_playback_transport_checked(
        &self,
        session_id: SessionId,
        reservation_epoch: Option<u64>,
        connection_id: u64,
        reconnect_deadline: Instant,
    ) -> Result<(), AdapterError> {
        let (voice, completions, flushed_generations, expiry_changed) = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&session_id.get())
                .ok_or_else(|| AdapterError::new("network media session is not reserved"))?;
            if reservation_epoch.is_some_and(|epoch| epoch != session.reservation_epoch) {
                return Ok(());
            }
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
                Arc::clone(&session.expiry_changed),
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
        expiry_changed.notify_one();
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
            if session.playback_finished {
                return Err(AdapterError::new("network media playback is finishing"));
            }
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
                    accepted_deadline: None,
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
        let (accepted_completion, rendered, _) = {
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
                    pending.accepted_deadline = Some(Instant::now() + self.operation_timeout);
                    let receipt =
                        PlaybackReceipt::new(pending.generation_id, PlaybackState::Accepted);
                    (
                        pending
                            .completion
                            .take()
                            .map(|completion| (completion, receipt)),
                        None,
                        None::<()>,
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
        let accepted = accepted_completion.is_some();
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
        if accepted {
            if let Some(session) = self.sessions.lock().await.get(&session_id.get()) {
                session.expiry_changed.notify_one();
            }
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
            if session.playback_finished {
                return Err(AdapterError::new("network media playback is finishing"));
            }
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

async fn accepted_expiry_worker(
    hub: NetworkMediaHub,
    session_id: SessionId,
    reservation_epoch: u64,
    cancellation: CancellationToken,
    shutdown: CancellationToken,
    changed: Arc<Notify>,
) {
    loop {
        let notified = changed.notified();
        let (deadline, expires_reconnect) = {
            let sessions = hub.sessions.lock().await;
            let Some(session) = sessions.get(&session_id.get()) else {
                return;
            };
            if session.reservation_epoch != reservation_epoch {
                return;
            }
            let accepted = session
                .pending_playback
                .values()
                .filter_map(|pending| pending.accepted_deadline)
                .min();
            let deadline = accepted.into_iter().chain(session.reconnect_deadline).min();
            let expires_reconnect =
                deadline.is_some_and(|deadline| session.reconnect_deadline == Some(deadline));
            (deadline, expires_reconnect)
        };
        match deadline {
            Some(deadline) => tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = shutdown.cancelled() => return,
                _ = notified => continue,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    let _ = hub.expire_accepted_generations(session_id, reservation_epoch, &cancellation).await;
                    if expires_reconnect {
                        hub.expire_waiting_playback(session_id, deadline).await;
                    }
                }
            },
            None => tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = shutdown.cancelled() => return,
                _ = notified => continue,
            },
        }
    }
}

impl NetworkMediaHub {
    async fn expire_accepted_generations(
        &self,
        session_id: SessionId,
        reservation_epoch: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), AdapterError> {
        let (voice, completions, generations) = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(&session_id.get()) else {
                return Ok(());
            };
            if session.reservation_epoch != reservation_epoch {
                return Ok(());
            }
            let now = Instant::now();
            let generations = session
                .pending_playback
                .values()
                .filter_map(|pending| {
                    pending
                        .accepted_deadline
                        .filter(|deadline| *deadline <= now)
                        .map(|_| pending.generation_id)
                })
                .collect::<HashSet<_>>();
            if generations.is_empty() {
                return Ok(());
            }
            let sequences = session
                .pending_playback
                .iter()
                .filter_map(|(sequence, pending)| {
                    generations
                        .contains(&pending.generation_id)
                        .then_some(*sequence)
                })
                .collect::<Vec<_>>();
            let mut completions = Vec::new();
            for sequence in sequences {
                if let Some(mut pending) = session.pending_playback.remove(&sequence) {
                    if let Some(completion) = pending.completion.take() {
                        completions.push(completion);
                    }
                }
            }
            (session.voice.clone(), completions, generations)
        };
        for completion in completions {
            let _ = completion.send(Err(AdapterError::new(
                "network media playback rendering timed out",
            )));
        }
        if let Some(voice) = voice {
            for generation_id in generations {
                tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    result = tokio::time::timeout(self.operation_timeout, voice.send(Ok(VoiceInputEvent::Playback(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))))) => {
                        result.map_err(|_| AdapterError::new("network media playback expiry event timed out"))?
                            .map_err(|_| AdapterError::new("network media voice input closed"))?;
                    }
                }
            }
        }
        Ok(())
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
