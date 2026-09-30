//! Connection-private ordering. Provider identities never become runtime IDs.
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::capture_evidence::{Ledger, Tracker};
use super::wire::protocol_error;
use crate::{AdapterError, AudioFrame, RecognitionEvent};

const MAX_ITEMS: usize = 32;
const MAX_RETIRED: usize = 64;
const MAX_ID_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_DELTA_REPLAYS: usize = 64;
const COUNTERPART_WAIT: Duration = Duration::from_secs(5);
const FINAL_WAIT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Item {
    predecessor: Option<Option<String>>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    delta: String,
    delta_replays: VecDeque<(String, [u8; 32])>,
    completed: Option<String>,
    orphan_at: Option<Instant>,
    stopped_at: Option<Instant>,
    committed_at: Option<Instant>,
    stop_missing: bool,
    capture: Tracker,
}

struct Retired {
    id: String,
    item: Item,
}

pub(super) struct Completed {
    pub(super) text: String,
    pub(super) start_ms: Option<u64>,
    pub(super) end_ms: Option<u64>,
    pub(super) capture: conversation_protocol::UtteranceCaptureEvidence,
}

#[derive(Default)]
pub(super) struct Items {
    pending: BTreeMap<String, Item>,
    retired: VecDeque<Retired>,
    root: Option<String>,
    released_tail: Option<String>,
    ledger: Ledger,
}

impl Items {
    pub(super) fn submitted(&mut self, frame: &AudioFrame) -> Result<(), AdapterError> {
        if let Some(evicted) = self.ledger.submit(frame)? {
            for item in self.pending.values_mut() {
                item.capture.evict(&evicted);
            }
        }
        Ok(())
    }
    pub(super) fn apply(
        &mut self,
        event: &Value,
        now: Instant,
    ) -> Result<Option<RecognitionEvent>, AdapterError> {
        let kind = event["type"]
            .as_str()
            .ok_or_else(|| protocol_error("missing event type"))?;
        if kind == "rate_limits.updated" {
            return Ok(None);
        }
        if kind == "error" {
            return Err(protocol_error("provider rejected transcription"));
        }
        if !matches!(
            kind,
            "input_audio_buffer.speech_started"
                | "input_audio_buffer.speech_stopped"
                | "input_audio_buffer.committed"
                | "conversation.item.input_audio_transcription.delta"
                | "conversation.item.input_audio_transcription.completed"
                | "conversation.item.input_audio_transcription.failed"
        ) {
            if kind.starts_with("conversation.item.input_audio_transcription.")
                || kind.starts_with("input_audio_buffer.")
                || kind.starts_with("session.")
            {
                return Err(protocol_error("unsupported transcription event"));
            }
            // Unrelated bounded provider notifications carry no ordering or
            // completion authority and cannot mutate pending item deadlines.
            return Ok(None);
        }
        let id = identifier(&event["item_id"])?;
        let transcription = kind.starts_with("conversation.item.input_audio_transcription.");
        if transcription {
            match event.get("content_index") {
                Some(index) if index.as_u64() == Some(0) => {}
                None if kind.ends_with(".delta")
                    && (self.pending.contains_key(id)
                        || self.retired.iter().any(|item| item.id == id)) => {}
                _ => return Err(protocol_error("unsupported or missing content index")),
            }
        }
        if kind.ends_with(".failed") {
            return Err(protocol_error("provider item failed"));
        }
        let predecessor = if kind == "input_audio_buffer.committed" {
            let value = event
                .get("previous_item_id")
                .ok_or_else(|| protocol_error("missing predecessor"))?;
            Some(if value.is_null() {
                None
            } else {
                Some(identifier(value)?.to_owned())
            })
        } else {
            None
        };
        if let Some(retired) = self.retired.iter().find(|item| item.id == id) {
            return validate_replay(&retired.item, event, kind, predecessor.as_ref())
                .map(|()| None);
        }
        if let Some(previous) = &predecessor {
            if previous.as_deref() == Some(id) {
                return Err(protocol_error("self predecessor"));
            }
            if previous.is_none() && self.root.as_deref().is_some_and(|root| root != id) {
                return Err(protocol_error("transcription chain was re-rooted"));
            }
            if self.pending.iter().any(|(other_id, item)| {
                other_id != id && item.predecessor.as_ref() == Some(previous)
            }) {
                return Err(protocol_error("transcription predecessor fork"));
            }
            if let Some(previous) = previous {
                if self.retired.iter().any(|item| item.id == *previous)
                    && self.released_tail.as_ref() != Some(previous)
                {
                    return Err(protocol_error("retired predecessor fork"));
                }
            }
        }
        if !self.pending.contains_key(id) && self.pending.len() >= MAX_ITEMS {
            return Err(protocol_error("unresolved item limit exceeded"));
        }
        let item = self.pending.entry(id.to_owned()).or_default();
        let mut activity = None;
        match kind {
            "input_audio_buffer.speech_started" => {
                let start = offset(event, "audio_start_ms")?;
                let first = item.start_ms.is_none();
                set_once(&mut item.start_ms, start)?;
                item.capture.start(start, &self.ledger);
                // Late metadata can improve capture evidence but cannot
                // resurrect speech after an observed or timed-out stop.
                if first && item.end_ms.is_none() && !item.stop_missing {
                    activity = Some(RecognitionEvent::SpeechStarted);
                }
            }
            "input_audio_buffer.speech_stopped" => {
                let end = offset(event, "audio_end_ms")?;
                let first = item.end_ms.is_none();
                set_once(&mut item.end_ms, end)?;
                item.capture.end(end, &self.ledger);
                item.stopped_at.get_or_insert(now);
                if first {
                    activity = Some(RecognitionEvent::SpeechEnded);
                }
            }
            "input_audio_buffer.committed" => {
                let previous = predecessor.expect("parsed committed predecessor");
                if let Some(existing) = &item.predecessor {
                    if existing != &previous {
                        return Err(protocol_error("conflicting predecessor"));
                    }
                } else {
                    if previous.is_none() {
                        self.root = Some(id.to_owned());
                    }
                    item.predecessor = Some(previous);
                    item.committed_at = Some(now);
                }
            }
            "conversation.item.input_audio_transcription.delta" => {
                let delta = text(event, "delta", true)?;
                let event_id = identifier(&event["event_id"])?;
                let fingerprint: [u8; 32] = Sha256::digest(delta.as_bytes()).into();
                if let Some((_, previous)) =
                    item.delta_replays.iter().find(|(id, _)| id == event_id)
                {
                    return if previous == &fingerprint {
                        Ok(None)
                    } else {
                        Err(protocol_error("conflicting delta replay"))
                    };
                }
                if item.delta_replays.len() == MAX_DELTA_REPLAYS {
                    item.delta_replays.pop_front();
                }
                item.delta_replays
                    .push_back((event_id.to_owned(), fingerprint));
                if item.completed.is_some() {
                    return Ok(None);
                }
                if item
                    .delta
                    .len()
                    .checked_add(delta.len())
                    .is_none_or(|len| len > MAX_TEXT_BYTES)
                {
                    return Err(protocol_error("delta text limit exceeded"));
                }
                item.delta.push_str(delta);
                item.orphan_at.get_or_insert(now);
            }
            "conversation.item.input_audio_transcription.completed" => {
                let completed = text(event, "transcript", false)?;
                if item
                    .completed
                    .as_deref()
                    .is_some_and(|previous| previous != completed)
                {
                    return Err(protocol_error("conflicting completed transcript"));
                }
                item.completed = Some(completed.to_owned());
                item.delta.clear();
                item.orphan_at.get_or_insert(now);
            }
            _ => unreachable!("relevant kinds checked above"),
        }
        Ok(activity)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter_map(|item| {
                if let Some(committed) = item.committed_at {
                    Some(
                        committed
                            + if item.end_ms.is_none() && !item.stop_missing {
                                COUNTERPART_WAIT
                            } else {
                                FINAL_WAIT
                            },
                    )
                } else {
                    item.orphan_at
                        .into_iter()
                        .chain(item.stopped_at)
                        .min()
                        .map(|at| at + COUNTERPART_WAIT)
                }
            })
            .min()
    }

    pub(super) fn ready(&mut self, now: Instant) -> Result<Vec<Completed>, AdapterError> {
        for item in self.pending.values_mut() {
            if let Some(committed) = item.committed_at {
                if now >= committed + FINAL_WAIT {
                    return Err(protocol_error(
                        "item completion or predecessor deadline expired",
                    ));
                }
                if item.end_ms.is_none() && now >= committed + COUNTERPART_WAIT {
                    item.stop_missing = true;
                }
            } else if item
                .orphan_at
                .into_iter()
                .chain(item.stopped_at)
                .min()
                .is_some_and(|at| now >= at + COUNTERPART_WAIT)
            {
                return Err(protocol_error("item commit deadline expired"));
            }
        }
        let mut completed = Vec::new();
        loop {
            let ready = self
                .pending
                .iter()
                .find(|(_, item)| {
                    item.predecessor.as_ref() == Some(&self.released_tail)
                        && item.completed.is_some()
                        && (item.end_ms.is_some() || item.stop_missing)
                })
                .map(|(id, _)| id.clone());
            let Some(id) = ready else {
                break;
            };
            let mut item = self.pending.remove(&id).expect("ready item exists");
            let text = item.completed.as_ref().expect("completed item").clone();
            completed.push(Completed {
                text,
                start_ms: item.start_ms,
                end_ms: item.end_ms,
                capture: item.capture.finish(&self.ledger),
            });
            item.capture = Tracker::default();
            item.delta.clear();
            item.delta_replays.clear();
            self.released_tail = Some(id.clone());
            if self.retired.len() == MAX_RETIRED {
                self.retired.pop_front();
            }
            self.retired.push_back(Retired { id, item });
        }
        Ok(completed)
    }
}

fn identifier(value: &Value) -> Result<&str, AdapterError> {
    value
        .as_str()
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
        })
        .ok_or_else(|| protocol_error("invalid provider identity"))
}

fn offset(event: &Value, key: &str) -> Result<u64, AdapterError> {
    event[key]
        .as_u64()
        .filter(|ms| ms.checked_mul(24).is_some())
        .ok_or_else(|| protocol_error("invalid provider sample offset"))
}

fn text<'a>(event: &'a Value, key: &str, allow_empty: bool) -> Result<&'a str, AdapterError> {
    event[key]
        .as_str()
        .filter(|text| text.len() <= MAX_TEXT_BYTES && (allow_empty || !text.trim().is_empty()))
        .ok_or_else(|| protocol_error("invalid bounded transcript"))
}

fn set_once(target: &mut Option<u64>, value: u64) -> Result<(), AdapterError> {
    if target.is_some_and(|previous| previous != value) {
        return Err(protocol_error("conflicting speech boundary"));
    }
    *target = Some(value);
    Ok(())
}

fn validate_replay(
    item: &Item,
    event: &Value,
    kind: &str,
    predecessor: Option<&Option<String>>,
) -> Result<(), AdapterError> {
    let matches = match kind {
        "input_audio_buffer.speech_started" => {
            item.start_ms == Some(offset(event, "audio_start_ms")?)
        }
        "input_audio_buffer.speech_stopped" => item.end_ms == Some(offset(event, "audio_end_ms")?),
        "input_audio_buffer.committed" => item.predecessor.as_ref() == predecessor,
        "conversation.item.input_audio_transcription.completed" => {
            item.completed.as_deref() == Some(text(event, "transcript", false)?)
        }
        "conversation.item.input_audio_transcription.delta" => {
            text(event, "delta", true)?;
            true
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(protocol_error("conflicting retired item replay"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(event_id: &str, value: &str) -> Value {
        json!({"type":"conversation.item.input_audio_transcription.delta", "event_id":event_id,
            "item_id":"A", "content_index":0, "delta":value})
    }

    #[test]
    fn speech_boundaries_emit_once_and_conflicts_never_emit_activity() {
        let now = Instant::now();
        let mut items = Items::default();
        let start =
            json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":0});
        let stop =
            json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20});
        assert_eq!(
            items.apply(&start, now).unwrap(),
            Some(RecognitionEvent::SpeechStarted)
        );
        assert_eq!(items.apply(&start, now).unwrap(), None);
        assert!(items.apply(&json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":1}), now).is_err());
        assert_eq!(
            items.apply(&stop, now).unwrap(),
            Some(RecognitionEvent::SpeechEnded)
        );
        assert_eq!(items.apply(&stop, now).unwrap(), None);
        assert!(items.apply(&json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":21}), now).is_err());
        assert_eq!(items.apply(&start, now).unwrap(), None);
        items.apply(&json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}), now).unwrap();
        items.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"kept"}), now).unwrap();
        assert_eq!(items.ready(now).unwrap().len(), 1);
        assert_eq!(items.apply(&start, now).unwrap(), None);
        assert_eq!(items.apply(&stop, now).unwrap(), None);
        assert!(items.apply(&json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":1}), now).is_err());
    }

    #[test]
    fn late_starts_after_stop_timeout_or_release_never_create_a_new_onset() {
        let now = Instant::now();
        let mut items = Items::default();
        let start =
            json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":0});
        let stop =
            json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20});
        assert_eq!(
            items.apply(&stop, now).unwrap(),
            Some(RecognitionEvent::SpeechEnded)
        );
        assert_eq!(items.apply(&start, now).unwrap(), None);
        // A complete orphan waits for its missing predecessor, even after the
        // missing-stop deadline. Metadata still cannot announce active speech.
        let mut items = Items::default();
        items.apply(&json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":"missing"}), now).unwrap();
        items.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"kept"}), now).unwrap();
        let later = now + COUNTERPART_WAIT;
        assert!(items.ready(later).unwrap().is_empty());
        assert_eq!(items.apply(&start, later).unwrap(), None);
        let mut items = Items::default();
        items.apply(&json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}), now).unwrap();
        items.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"kept"}), now).unwrap();
        assert_eq!(items.ready(later).unwrap().len(), 1);
        assert!(
            items.apply(&start, later).is_err(),
            "an unseen boundary for a retired item is conflicting replay"
        );
    }

    #[test]
    fn recent_delta_replay_is_idempotent_but_distinct_ids_can_repeat_text() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&delta("one", "echo"), now).unwrap();
        items.apply(&delta("one", "echo"), now).unwrap();
        items.apply(&delta("two", "echo"), now).unwrap();
        assert_eq!(items.pending["A"].delta, "echoecho");
        assert!(items.apply(&delta("one", "conflict"), now).is_err());
    }

    #[test]
    fn delta_replay_window_is_exactly_bounded_and_cannot_change_completed_text() {
        let now = Instant::now();
        let mut items = Items::default();
        for index in 0..64 {
            items.apply(&delta(&format!("d{index}"), "x"), now).unwrap();
        }
        items.apply(&delta("d0", "x"), now).unwrap();
        assert_eq!(
            items.pending["A"].delta.len(),
            64,
            "oldest retained replay must not append"
        );
        items.apply(&delta("d64", "x"), now).unwrap();
        items.apply(&delta("d0", "x"), now).unwrap();
        assert_eq!(
            items.pending["A"].delta.len(),
            66,
            "evicted replay has only provisional effect"
        );
        for event in [
            json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}),
            json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20}),
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"authoritative"}),
        ] {
            items.apply(&event, now).unwrap();
        }
        let ready = items.ready(now).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].text, "authoritative");
        assert!(items.ready(now).unwrap().is_empty());
    }

    #[test]
    fn replay_and_progress_never_refresh_missing_commit_deadline() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&delta("one", "x"), now).unwrap();
        items
            .apply(&delta("one", "x"), now + Duration::from_secs(4))
            .unwrap();
        items
            .apply(&delta("two", "x"), now + Duration::from_secs(4))
            .unwrap();
        assert!(items.ready(now + Duration::from_secs(5)).is_err());
    }

    #[test]
    fn unrelated_bounded_events_do_not_clear_or_extend_pending_items() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&delta("one", "x"), now).unwrap();
        items
            .apply(
                &json!({"type":"conversation.item.added", "item":{"id":"A","role":"user"}}),
                now + Duration::from_secs(4),
            )
            .unwrap();
        assert!(items.ready(now + Duration::from_secs(5)).is_err());
    }

    #[test]
    fn long_speech_alone_has_no_timer_but_stop_without_commit_expires() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":0}), now).unwrap();
        assert!(items.deadline().is_none());
        assert!(items
            .ready(now + Duration::from_secs(600))
            .unwrap()
            .is_empty());
        items.apply(&json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":600000}), now + Duration::from_secs(600)).unwrap();
        assert!(items.ready(now + Duration::from_secs(605)).is_err());
    }

    #[test]
    fn missing_stop_releases_ordered_text_only_after_the_fixed_counterpart_wait() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}), now).unwrap();
        items.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"kept"}), now).unwrap();
        assert!(items
            .ready(now + Duration::from_secs(4))
            .unwrap()
            .is_empty());
        let finals = items.ready(now + Duration::from_secs(5)).unwrap();
        assert_eq!(finals[0].text, "kept");
        assert_eq!(finals[0].end_ms, None);
    }

    #[test]
    fn missing_predecessor_expires_without_arrival_order_guessing() {
        let now = Instant::now();
        let mut items = Items::default();
        items.apply(&json!({"type":"input_audio_buffer.committed","item_id":"B","previous_item_id":"A"}), now).unwrap();
        items.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"B","content_index":0,"transcript":"second"}), now).unwrap();
        assert!(items
            .ready(now + Duration::from_secs(5))
            .unwrap()
            .is_empty());
        assert!(items.ready(now + Duration::from_secs(30)).is_err());
    }

    #[test]
    fn item_identity_text_and_content_limits_are_checked_before_admission() {
        let now = Instant::now();
        for event in [
            json!({"type":"input_audio_buffer.committed","item_id":"A"}),
            json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":"A"}),
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","transcript":"missing index"}),
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":1,"transcript":"unsupported index"}),
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"x".repeat(MAX_TEXT_BYTES+1)}),
            json!({"type":"input_audio_buffer.speech_started","item_id":"x".repeat(MAX_ID_BYTES+1),"audio_start_ms":0}),
            json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":u64::MAX}),
        ] {
            assert!(Items::default().apply(&event, now).is_err());
        }
        let mut items = Items::default();
        for index in 0..MAX_ITEMS {
            items.apply(&json!({"type":"input_audio_buffer.speech_started","item_id":format!("i{index}"),"audio_start_ms":0}), now).unwrap();
        }
        assert!(items.apply(&json!({"type":"input_audio_buffer.speech_started","item_id":"overflow","audio_start_ms":0}), now).is_err());
    }
}
