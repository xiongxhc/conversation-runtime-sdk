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
/// Segment ids are session-unique (the reader renumbers the sidecar's per-turn
/// ids). The handle holds at most one turn's audio, for the current session,
/// replaced by the next turn's. A request takes the audio when its segment id is
/// the audio's; it gets nothing at once when that segment's turn, or the
/// session, has already ended without that audio; otherwise it waits.
#[derive(Clone, Default)]
pub struct SidecarTurnAudio {
    inner: Arc<TurnAudioSlot>,
}

#[derive(Default)]
struct TurnAudioSlot {
    state: Mutex<Option<SessionTurnAudio>>,
    changed: Notify,
}

/// The current session's state; `None` before any session begins.
struct SessionTurnAudio {
    session_id: SessionId,
    held: Option<(u64, CapturedTurnAudio)>,
    /// Every segment id at or below this belongs to a turn that has ended.
    ended_through: u64,
    session_ended: bool,
}

enum Answer {
    Audio(CapturedTurnAudio),
    Nothing,
    Wait,
}

impl SidecarTurnAudio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for the audio of `segment_id` and takes it. Returns `None` once
    /// `cancellation` fires, or at once when the segment's turn or the session
    /// has ended without that audio.
    pub async fn take(
        &self,
        session_id: SessionId,
        segment_id: u64,
        cancellation: CancellationToken,
    ) -> Option<CapturedTurnAudio> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.answer(session_id, segment_id) {
                Answer::Audio(audio) => return Some(audio),
                Answer::Nothing => return None,
                Answer::Wait => {}
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return None,
                () = &mut changed => {}
            }
        }
    }

    /// A sidecar session's reader has started; anything held for an earlier
    /// session, including one with the same id, is dropped.
    pub(crate) fn begin(&self, session_id: SessionId) {
        *self.state() = Some(SessionTurnAudio {
            session_id,
            held: None,
            ended_through: 0,
            session_ended: false,
        });
        self.inner.changed.notify_waiters();
    }

    /// A finished turn's audio, replacing the previous turn's. Its turn has
    /// ended, so every earlier segment id has too.
    pub(crate) fn offer(&self, session_id: SessionId, segment_id: u64, audio: CapturedTurnAudio) {
        self.update(session_id, |current| {
            current.ended_through = current.ended_through.max(segment_id);
            current.held = Some((segment_id, audio));
        });
    }

    /// A turn ended; it used segment ids up to `through_segment_id`.
    pub(crate) fn turn_ended(&self, session_id: SessionId, through_segment_id: u64) {
        self.update(session_id, |current| {
            current.ended_through = current.ended_through.max(through_segment_id);
        });
    }

    /// The session's reader has stopped: drop its audio and end its requests.
    pub(crate) fn end(&self, session_id: SessionId) {
        self.update(session_id, |current| {
            current.held = None;
            current.session_ended = true;
        });
    }

    /// Applies `change` only to the current session's state.
    fn update(&self, session_id: SessionId, change: impl FnOnce(&mut SessionTurnAudio)) {
        if let Some(current) = self
            .state()
            .as_mut()
            .filter(|current| current.session_id == session_id)
        {
            change(current);
        }
        self.inner.changed.notify_waiters();
    }

    fn answer(&self, session_id: SessionId, segment_id: u64) -> Answer {
        let mut state = self.state();
        let Some(current) = state.as_mut() else {
            return Answer::Wait;
        };
        if current.session_id != session_id {
            // Only the current session's audio is ever held.
            return Answer::Nothing;
        }
        if current
            .held
            .as_ref()
            .is_some_and(|(held, _)| *held == segment_id)
        {
            if let Some((_, audio)) = current.held.take() {
                return Answer::Audio(audio);
            }
        }
        if current.session_ended || segment_id <= current.ended_through {
            Answer::Nothing
        } else {
            Answer::Wait
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Option<SessionTurnAudio>> {
        self.inner
            .state
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
    const OTHER: SessionId = SessionId::new(8);

    fn audio(value: f32) -> CapturedTurnAudio {
        CapturedTurnAudio {
            samples: vec![value; 4],
            playback_overlap: false,
            single_segment: true,
        }
    }

    fn session() -> SidecarTurnAudio {
        let handle = SidecarTurnAudio::new();
        handle.begin(SESSION);
        handle
    }

    /// The answer a take gives without waiting, or `None` when it would wait.
    fn answer_now(
        handle: &SidecarTurnAudio,
        session_id: SessionId,
        segment_id: u64,
    ) -> Option<Option<CapturedTurnAudio>> {
        handle
            .take(session_id, segment_id, CancellationToken::new())
            .now_or_never()
    }

    #[tokio::test]
    async fn take_returns_audio_offered_before_or_after_the_request() {
        let handle = session();
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(Some(audio(0.25))));

        let take = handle.take(SESSION, 2, CancellationToken::new());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(SESSION, 2, audio(0.5));
        assert_eq!(take.now_or_never(), Some(Some(audio(0.5))));
    }

    #[tokio::test]
    async fn a_late_request_never_receives_a_later_turns_audio() {
        let handle = session();
        handle.offer(SESSION, 1, audio(0.25));
        handle.offer(SESSION, 2, audio(0.5));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 2), Some(Some(audio(0.5))));

        // A request already waiting when a later turn's audio arrives.
        let waiting = handle.take(SESSION, 3, CancellationToken::new());
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        handle.offer(SESSION, 4, audio(0.75));
        assert_eq!(waiting.now_or_never(), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 4), Some(Some(audio(0.75))));
    }

    #[tokio::test]
    async fn a_request_for_a_turn_that_ended_without_audio_gets_nothing_at_once() {
        let handle = session();
        let waiting = handle.take(SESSION, 3, CancellationToken::new());
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());

        handle.turn_ended(SESSION, 3);
        assert_eq!(waiting.now_or_never(), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 3), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(None));
        // A later turn's request still waits for its audio.
        assert_eq!(answer_now(&handle, SESSION, 4), None);
    }

    #[tokio::test]
    async fn the_end_of_a_turn_keeps_that_turns_audio() {
        let handle = session();
        handle.offer(SESSION, 2, audio(0.25));
        handle.turn_ended(SESSION, 3);
        assert_eq!(answer_now(&handle, SESSION, 2), Some(Some(audio(0.25))));
    }

    #[tokio::test]
    async fn audio_does_not_outlive_its_session() {
        let handle = session();
        handle.offer(SESSION, 1, audio(0.25));
        let waiting = handle.take(SESSION, 2, CancellationToken::new());
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());

        handle.end(SESSION);
        assert_eq!(waiting.now_or_never(), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 5), Some(None));
    }

    #[tokio::test]
    async fn a_new_session_with_the_same_id_starts_afresh() {
        let handle = session();
        handle.offer(SESSION, 3, audio(0.25));
        handle.end(SESSION);
        handle.begin(SESSION);

        let take = handle.take(SESSION, 1, CancellationToken::new());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(SESSION, 1, audio(0.5));
        assert_eq!(take.now_or_never(), Some(Some(audio(0.5))));
    }

    #[tokio::test]
    async fn a_stale_sessions_end_leaves_the_current_session_alone() {
        let handle = session();
        handle.offer(SESSION, 1, audio(0.25));
        handle.end(OTHER);
        handle.turn_ended(OTHER, 9);
        assert_eq!(answer_now(&handle, SESSION, 1), Some(Some(audio(0.25))));
    }

    #[tokio::test]
    async fn a_different_segment_or_session_is_never_returned() {
        let handle = session();
        let cancellation = CancellationToken::new();
        let take = handle.take(SESSION, 3, cancellation.clone());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());

        handle.offer(SESSION, 2, audio(0.25));
        assert!(take.as_mut().now_or_never().is_none());
        handle.offer(OTHER, 3, audio(0.5));
        assert!(take.as_mut().now_or_never().is_none());
        // Only the current session's audio is ever held.
        assert_eq!(answer_now(&handle, OTHER, 3), Some(None));

        cancellation.cancel();
        assert_eq!(take.now_or_never(), Some(None));
        assert_eq!(answer_now(&handle, SESSION, 2), Some(Some(audio(0.25))));
    }

    #[tokio::test]
    async fn audio_is_taken_once() {
        let handle = session();
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(Some(audio(0.25))));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(None));
    }

    #[tokio::test]
    async fn cancellation_ends_a_waiting_take() {
        let handle = session();
        let cancellation = CancellationToken::new();
        let take = handle.take(SESSION, 1, cancellation.clone());
        tokio::pin!(take);
        assert!(take.as_mut().now_or_never().is_none());

        cancellation.cancel();
        assert_eq!(take.now_or_never(), Some(None));

        // A cancelled take left nothing behind: later audio reaches a new take.
        handle.offer(SESSION, 1, audio(0.25));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(Some(audio(0.25))));
    }

    #[tokio::test]
    async fn a_clone_shares_the_slot() {
        let handle = session();
        let reader = handle.clone();
        reader.offer(SESSION, 1, audio(0.25));
        assert_eq!(answer_now(&handle, SESSION, 1), Some(Some(audio(0.25))));
    }

    #[test]
    fn debug_output_never_contains_samples() {
        let rendered = format!("{:?}", audio(0.123_456));
        assert!(!rendered.contains("0.123"));
        assert!(rendered.contains("sample_count: 4"));
    }
}
