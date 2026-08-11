use conversation_model_adapters::RecognitionHypothesis;
use conversation_protocol::{RuntimeError, RuntimeErrorKind, RuntimeStage, VoiceActivity};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedTranscript {
    pub segment_id: u64,
    pub text: String,
}

#[derive(Debug)]
pub struct TurnFinalizer {
    final_silence_ms: u64,
    segment_id: Option<u64>,
    committed: Vec<String>,
    display_candidate: Option<String>,
    engine_final_candidate: Option<String>,
    speech_ended_at_ms: Option<u64>,
    finalized: bool,
}

impl TurnFinalizer {
    pub fn new(final_silence_ms: u64) -> Result<Self, RuntimeError> {
        if final_silence_ms == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Configuration,
                RuntimeStage::Runtime,
                "turn finalization silence duration must be non-zero",
            ));
        }

        Ok(Self {
            final_silence_ms,
            segment_id: None,
            committed: Vec::new(),
            display_candidate: None,
            engine_final_candidate: None,
            speech_ended_at_ms: None,
            finalized: false,
        })
    }

    pub fn observe_hypothesis(&mut self, value: RecognitionHypothesis, _at_ms: u64) -> bool {
        if self.finalized || value.text().trim().is_empty() {
            return false;
        }

        let segment_id = value.segment_id();
        let text = value.text().to_owned();

        if self.segment_id != Some(segment_id) {
            if let Some(committed) = self.engine_final_candidate.take() {
                self.committed.push(committed);
            }
            self.segment_id = Some(segment_id);
            self.engine_final_candidate = None;
        }

        self.display_candidate = Some(text.clone());
        if value.is_engine_final() {
            self.engine_final_candidate = Some(text);
        }
        true
    }

    pub fn observe_activity(&mut self, value: VoiceActivity) {
        match value {
            VoiceActivity::SpeechStarted { .. } => {
                if self.finalized {
                    self.segment_id = None;
                    self.committed.clear();
                    self.display_candidate = None;
                    self.engine_final_candidate = None;
                    self.finalized = false;
                }
                self.speech_ended_at_ms = None;
            }
            VoiceActivity::SpeechContinued { .. } => {
                self.speech_ended_at_ms = None;
            }
            VoiceActivity::SpeechEnded { at_ms } => {
                self.speech_ended_at_ms = Some(at_ms);
            }
            _ => {}
        }
    }

    pub fn display_text(&self) -> Option<&str> {
        self.display_candidate.as_deref()
    }

    pub(crate) fn remaining_silence_ms(&self, now_ms: u64) -> Option<u64> {
        if self.finalized {
            return None;
        }

        self.speech_ended_at_ms.map(|ended_at_ms| {
            ended_at_ms
                .saturating_add(self.final_silence_ms)
                .saturating_sub(now_ms)
        })
    }

    pub fn finalize_ready(&mut self, now_ms: u64) -> Option<FinalizedTranscript> {
        if self.finalized {
            return None;
        }

        let segment_id = self.segment_id?;
        let speech_ended_at_ms = self.speech_ended_at_ms?;
        if now_ms < speech_ended_at_ms.saturating_add(self.final_silence_ms) {
            return None;
        }

        let candidate = self.engine_final_candidate.as_ref()?;
        let mut text: String = self.committed.concat();
        text.push_str(candidate);
        if text.trim().is_empty() {
            return None;
        }

        self.finalized = true;
        Some(FinalizedTranscript { segment_id, text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finalizer() -> TurnFinalizer {
        TurnFinalizer::new(600).unwrap()
    }

    fn ended(finalizer: &mut TurnFinalizer, at_ms: u64) {
        finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms });
    }

    #[test]
    fn accumulates_engine_final_segments_across_segment_ids() {
        let mut finalizer = finalizer();
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(1, " first half"), 0);
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(2, " second half."), 100);
        ended(&mut finalizer, 100);

        let finalized = finalizer.finalize_ready(700).expect("utterance finalizes");
        assert_eq!(finalized.text, " first half second half.");
        assert_eq!(finalized.segment_id, 2);
    }

    #[test]
    fn late_segment_does_not_clear_the_silence_gate() {
        let mut finalizer = finalizer();
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(1, " early"), 0);
        ended(&mut finalizer, 0);
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(2, " late"), 500);

        let finalized = finalizer
            .finalize_ready(1_100)
            .expect("late segment must not stall finalization");
        assert_eq!(finalized.text, " early late");
    }

    #[test]
    fn remaining_silence_tracks_the_runtime_clock_and_speech_resume() {
        let mut finalizer = finalizer();

        assert_eq!(finalizer.remaining_silence_ms(100), None);

        ended(&mut finalizer, 100);
        assert_eq!(finalizer.remaining_silence_ms(100), Some(600));
        assert_eq!(finalizer.remaining_silence_ms(699), Some(1));
        assert_eq!(finalizer.remaining_silence_ms(700), Some(0));

        finalizer.observe_activity(VoiceActivity::SpeechStarted { at_ms: 800 });
        assert_eq!(finalizer.remaining_silence_ms(800), None);

        ended(&mut finalizer, u64::MAX - 100);
        assert_eq!(finalizer.remaining_silence_ms(u64::MAX - 1), Some(1));
        assert_eq!(finalizer.remaining_silence_ms(u64::MAX), Some(0));
    }

    #[test]
    fn pending_partial_segment_defers_finalization_until_engine_final() {
        let mut finalizer = finalizer();
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(1, " early"), 0);
        ended(&mut finalizer, 0);
        finalizer.observe_hypothesis(RecognitionHypothesis::partial(2, " la"), 500);

        assert!(finalizer.finalize_ready(1_100).is_none());

        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(2, " late"), 900);
        let finalized = finalizer
            .finalize_ready(1_200)
            .expect("utterance finalizes");
        assert_eq!(finalized.text, " early late");
    }

    #[test]
    fn next_utterance_starts_without_previous_segments() {
        let mut finalizer = finalizer();
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(1, " first"), 0);
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(2, " utterance"), 100);
        ended(&mut finalizer, 100);
        assert!(finalizer.finalize_ready(700).is_some());

        finalizer.observe_activity(VoiceActivity::SpeechStarted { at_ms: 2_000 });
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(3, " next"), 2_000);
        ended(&mut finalizer, 2_000);
        let finalized = finalizer
            .finalize_ready(2_600)
            .expect("next utterance finalizes");
        assert_eq!(finalized.text, " next");
    }

    #[test]
    fn late_hypothesis_waits_for_the_next_speech_start() {
        let mut finalizer = finalizer();
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(1, " first"), 0);
        ended(&mut finalizer, 0);
        assert!(finalizer.finalize_ready(600).is_some());

        assert!(
            !finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(2, " stale"), 700)
        );
        assert!(finalizer.finalize_ready(1_300).is_none());

        finalizer.observe_activity(VoiceActivity::SpeechStarted { at_ms: 2_000 });
        finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(3, " next"), 2_100);
        ended(&mut finalizer, 2_100);
        let finalized = finalizer
            .finalize_ready(2_700)
            .expect("new speech starts the next utterance");
        assert_eq!(finalized.text, " next");
    }
}
