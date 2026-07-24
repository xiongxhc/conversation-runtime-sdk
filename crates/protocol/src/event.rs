use crate::{RuntimeError, TurnId};

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
    SpeechStarted {
        turn_id: TurnId,
    },
    SpeechCompleted {
        turn_id: TurnId,
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
            | Self::SpeechStarted { turn_id }
            | Self::SpeechCompleted { turn_id }
            | Self::TurnCompleted { turn_id }
            | Self::TurnCancelled { turn_id }
            | Self::TurnFailed { turn_id, .. } => *turn_id,
        }
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TurnCompleted { .. } | Self::TurnCancelled { .. } | Self::TurnFailed { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeEvent;
    use crate::TurnId;

    #[test]
    fn only_terminal_events_report_terminal_state() {
        let turn_id = TurnId::new(1);

        assert!(!RuntimeEvent::TurnStarted { turn_id }.is_terminal());
        assert!(RuntimeEvent::TurnCompleted { turn_id }.is_terminal());
        assert!(RuntimeEvent::TurnCancelled { turn_id }.is_terminal());
    }
}
