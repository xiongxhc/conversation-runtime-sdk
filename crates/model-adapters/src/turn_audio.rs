use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use conversation_protocol::SessionId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A finished voice turn's audio: 16 kHz mono samples in [-1, 1].
#[derive(Clone, PartialEq)]
pub struct CapturedTurnAudio {
    pub samples: Vec<f32>,
    pub playback_overlap: bool,
    pub single_segment: bool,
}

impl fmt::Debug for CapturedTurnAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedTurnAudio")
            .field("sample_count", &self.samples.len())
            .field("playback_overlap", &self.playback_overlap)
            .field("single_segment", &self.single_segment)
            .finish()
    }
}

/// The most recent turn's audio from a version 2 sidecar session.
///
/// Sidecar segment ids restart on every turn, so the handle holds at most one
/// turn: a transcript hypothesis discards it, a later turn's audio replaces it,
/// and a request takes it only when the session and segment match.
#[derive(Clone, Default)]
pub struct SidecarTurnAudio {
    inner: Arc<TurnAudioSlot>,
}

#[derive(Default)]
struct TurnAudioSlot {
    held: Mutex<Option<HeldTurnAudio>>,
    offered: Notify,
}

struct HeldTurnAudio {
    session_id: SessionId,
    segment_id: u64,
    audio: CapturedTurnAudio,
}

impl SidecarTurnAudio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for the audio of `segment_id` and takes it, or returns `None`
    /// once `cancellation` fires.
    pub async fn take(
        &self,
        session_id: SessionId,
        segment_id: u64,
        cancellation: CancellationToken,
    ) -> Option<CapturedTurnAudio> {
        loop {
            let offered = self.inner.offered.notified();
            tokio::pin!(offered);
            offered.as_mut().enable();
            if let Some(audio) = self.take_held(session_id, segment_id) {
                return Some(audio);
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return None,
                () = &mut offered => {}
            }
        }
    }

    pub(crate) fn offer(&self, session_id: SessionId, segment_id: u64, audio: CapturedTurnAudio) {
        *self.held() = Some(HeldTurnAudio {
            session_id,
            segment_id,
            audio,
        });
        self.inner.offered.notify_waiters();
    }

    pub(crate) fn clear(&self, session_id: SessionId) {
        let mut held = self.held();
        if held
            .as_ref()
            .is_some_and(|held| held.session_id == session_id)
        {
            *held = None;
        }
    }

    fn take_held(&self, session_id: SessionId, segment_id: u64) -> Option<CapturedTurnAudio> {
        let mut held = self.held();
        if held
            .as_ref()
            .is_some_and(|held| held.session_id == session_id && held.segment_id == segment_id)
        {
            held.take().map(|held| held.audio)
        } else {
            None
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Option<HeldTurnAudio>> {
        self.inner
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for SidecarTurnAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SidecarTurnAudio")
            .finish_non_exhaustive()
    }
}

/// Two handles are equal when they share one slot.
impl PartialEq for SidecarTurnAudio {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for SidecarTurnAudio {}

#[cfg(test)]
mod tests {
    use conversation_protocol::SessionId;
    use futures_util::FutureExt;
    use tokio_util::sync::CancellationToken;

    use super::{CapturedTurnAudio, SidecarTurnAudio};

    const SESSION: SessionId = SessionId::new(7);

    fn audio(value: f32) -> CapturedTurnAudio {
        CapturedTurnAudio {
            samples: vec![value; 4],
            playback_overlap: false,
            single_segment: true,
        }
    }

    #[tokio::test]
    async fn take_returns_audio_offered_before_or_after_the_request() {
        let handle = SidecarTurnAudio::new();
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(
            handle.take(SESSION, 1, CancellationToken::new()).await,
            Some(audio(0.25))
        );

        let take = handle.take(SESSION, 2, CancellationToken::new());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(SESSION, 2, audio(0.5));
        assert_eq!(take.await, Some(audio(0.5)));
    }

    #[tokio::test]
    async fn a_hypothesis_makes_earlier_audio_unavailable() {
        let handle = SidecarTurnAudio::new();
        handle.offer(SESSION, 1, audio(0.25));
        handle.clear(SESSION);

        let take = handle.take(SESSION, 1, CancellationToken::new());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(SESSION, 1, audio(0.75));
        assert_eq!(take.await, Some(audio(0.75)));
    }

    #[tokio::test]
    async fn clearing_another_session_keeps_this_sessions_audio() {
        let handle = SidecarTurnAudio::new();
        handle.offer(SESSION, 1, audio(0.25));
        handle.clear(SessionId::new(8));
        assert_eq!(
            handle.take(SESSION, 1, CancellationToken::new()).await,
            Some(audio(0.25))
        );
    }

    #[tokio::test]
    async fn a_different_segment_or_session_is_never_returned() {
        let handle = SidecarTurnAudio::new();
        let cancellation = CancellationToken::new();
        let take = handle.take(SESSION, 3, cancellation.clone());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());

        handle.offer(SESSION, 2, audio(0.25));
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(SessionId::new(8), 3, audio(0.5));
        assert!(take.as_mut().now_or_never().is_none());

        cancellation.cancel();
        assert_eq!(take.await, None);
        // The non-matching audio was left in place, not consumed.
        assert_eq!(
            handle
                .take(SessionId::new(8), 3, CancellationToken::new())
                .await,
            Some(audio(0.5))
        );
    }

    #[tokio::test]
    async fn audio_is_taken_once() {
        let handle = SidecarTurnAudio::new();
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(
            handle.take(SESSION, 1, CancellationToken::new()).await,
            Some(audio(0.25))
        );

        let take = handle.take(SESSION, 1, CancellationToken::new());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());
    }

    #[tokio::test]
    async fn cancellation_ends_a_waiting_take() {
        let handle = SidecarTurnAudio::new();
        let cancellation = CancellationToken::new();
        let take = handle.take(SESSION, 1, cancellation.clone());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());

        cancellation.cancel();
        assert_eq!(take.await, None);

        // A cancelled take left nothing behind: later audio reaches a new take.
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(
            handle.take(SESSION, 1, CancellationToken::new()).await,
            Some(audio(0.25))
        );
    }

    #[tokio::test]
    async fn a_clone_shares_the_slot() {
        let handle = SidecarTurnAudio::new();
        let reader = handle.clone();
        reader.offer(SESSION, 1, audio(0.25));
        assert_eq!(
            handle.take(SESSION, 1, CancellationToken::new()).await,
            Some(audio(0.25))
        );
    }

    #[test]
    fn debug_output_never_contains_samples() {
        let rendered = format!("{:?}", audio(0.123_456));
        assert!(!rendered.contains("0.123"));
        assert!(rendered.contains("sample_count: 4"));
    }
}
