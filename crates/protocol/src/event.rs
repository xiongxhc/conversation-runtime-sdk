use crate::{GenerationId, PlaybackState, QualityDecision, RuntimeError, TurnId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeTimingMilestone {
    FirstTextDelta,
    FirstSynthesisRequest,
    FirstPlayableAudio,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    TranscriptFinal {
        turn_id: TurnId,
        text: String,
    },
    TextDelta {
        turn_id: TurnId,
        delta: String,
    },
    Timing {
        turn_id: TurnId,
        milestone: RuntimeTimingMilestone,
        elapsed_ms: u64,
    },
    SpeechStarted {
        turn_id: TurnId,
    },
    SpeechCompleted {
        turn_id: TurnId,
    },
    Playback {
        turn_id: TurnId,
        generation_id: GenerationId,
        state: PlaybackState,
    },
    QualityResolved {
        decision: QualityDecision,
    },
    TurnCompleted {
        turn_id: TurnId,
    },
    TurnCancelled {
        turn_id: TurnId,
    },
    TurnFailed {
        turn_id: TurnId,
        error: RuntimeError,
    },
}

impl RuntimeEvent {
    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::TurnStarted { turn_id }
            | Self::TranscriptFinal { turn_id, .. }
            | Self::TextDelta { turn_id, .. }
            | Self::Timing { turn_id, .. }
            | Self::SpeechStarted { turn_id }
            | Self::SpeechCompleted { turn_id }
            | Self::Playback { turn_id, .. }
            | Self::TurnCompleted { turn_id }
            | Self::TurnCancelled { turn_id }
            | Self::TurnFailed { turn_id, .. } => *turn_id,
            Self::QualityResolved { decision } => decision.turn_id(),
        }
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TurnCompleted { .. } | Self::TurnCancelled { .. } | Self::TurnFailed { .. }
        )
    }

    pub fn quality_metric_json(&self) -> Option<String> {
        match self {
            Self::QualityResolved { decision } => Some(format!(
                "{{\"event\":\"quality_resolved\",\"decision\":{}}}",
                decision.metric_json()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{GenerationId, PlaybackState, RuntimeEvent, RuntimeTimingMilestone, TurnId};

    #[test]
    fn only_terminal_events_report_terminal_state() {
        let turn_id = TurnId::new(1);

        assert!(!RuntimeEvent::TurnStarted { turn_id }.is_terminal());
        assert!(RuntimeEvent::TurnCompleted { turn_id }.is_terminal());
        assert!(RuntimeEvent::TurnCancelled { turn_id }.is_terminal());
    }

    #[test]
    fn public_timing_events_preserve_turn_and_are_nonterminal() {
        let turn_id = TurnId::new(9);

        for milestone in [
            RuntimeTimingMilestone::FirstTextDelta,
            RuntimeTimingMilestone::FirstSynthesisRequest,
            RuntimeTimingMilestone::FirstPlayableAudio,
        ] {
            let event = RuntimeEvent::Timing {
                turn_id,
                milestone,
                elapsed_ms: 42,
            };

            assert_eq!(event.turn_id(), turn_id);
            assert!(!event.is_terminal());
        }
    }

    #[test]
    fn playback_acceptance_preserves_turn_and_generation_identity() {
        let turn_id = TurnId::new(9);
        let generation_id = GenerationId::new(10);
        let event = RuntimeEvent::Playback {
            turn_id,
            generation_id,
            state: PlaybackState::Accepted,
        };

        assert_eq!(event.turn_id(), turn_id);
        assert!(!event.is_terminal());
    }
}
