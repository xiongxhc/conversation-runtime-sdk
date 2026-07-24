use crate::TurnId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCommand {
    StartTurn { turn_id: TurnId, transcript: String },
    Interrupt { turn_id: TurnId },
}
