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
            display_candidate: None,
            engine_final_candidate: None,
            speech_ended_at_ms: None,
            finalized: false,
        })
    }

    pub fn observe_hypothesis(&mut self, value: RecognitionHypothesis, _at_ms: u64) {
        let segment_id = value.segment_id();
        let text = value.text().to_owned();

        if self.segment_id != Some(segment_id) {
            if self.segment_id.is_some() {
                self.speech_ended_at_ms = None;
            }
            self.segment_id = Some(segment_id);
            self.engine_final_candidate = None;
            self.finalized = false;
        }

        self.display_candidate = Some(text.clone());
        if value.is_engine_final() {
            self.engine_final_candidate = Some(text);
        }
    }

    pub fn observe_activity(&mut self, value: VoiceActivity) {
        match value {
            VoiceActivity::SpeechStarted { .. } | VoiceActivity::SpeechContinued { .. } => {
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

    pub fn finalize_ready(&mut self, now_ms: u64) -> Option<FinalizedTranscript> {
        if self.finalized {
            return None;
        }

        let segment_id = self.segment_id?;
        let speech_ended_at_ms = self.speech_ended_at_ms?;
        if now_ms < speech_ended_at_ms.saturating_add(self.final_silence_ms) {
            return None;
        }

        let text = self.engine_final_candidate.as_ref()?;
        if text.trim().is_empty() {
            return None;
        }

        self.finalized = true;
        Some(FinalizedTranscript {
            segment_id,
            text: text.clone(),
        })
    }
}
