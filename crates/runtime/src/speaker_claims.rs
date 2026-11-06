//! What a spoken "I'm <name>" means, given who the voice matches and who already has the
//! name. Wrongly naming someone is worse than asking, so every row that is not certain asks.

use conversation_protocol::PersonId;

use crate::VoiceEvidence;

/// Someone who already has the claimed name, as a name or an alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimHolder {
    pub person: PersonId,
    pub mature: bool,
    pub owner: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimDecision {
    /// An unknown voice with a free name: enrol it under that name.
    Enrol,
    Confirmed(PersonId),
    /// The voice is someone else's, and the name belongs to another person.
    Denied {
        speaker: PersonId,
    },
    /// A known voice giving a new name: ask whether it is another name for them.
    PossibleAlias {
        person: PersonId,
    },
    /// Ask "is that you, <person>?".
    AskIsThisPerson {
        person: PersonId,
    },
    /// Ask whether the speaker is someone new with the claimed name.
    AskIsNamed,
}

/// `matched_is_mature` describes the person a confident or possible match names and is
/// ignored for an unknown voice. Only a mature voice may deny, and an unconfirmed voice is
/// never asked about the owner: a yes there would link a voice to the owner.
pub fn decide_claim(
    evidence: &VoiceEvidence,
    matched_is_mature: bool,
    holders: &[ClaimHolder],
) -> ClaimDecision {
    if let VoiceEvidence::Confident { person, .. } = *evidence {
        if holders.iter().any(|holder| holder.person == person) {
            return ClaimDecision::Confirmed(person);
        }
        return match (matched_is_mature, holders.is_empty()) {
            (true, false) => ClaimDecision::Denied { speaker: person },
            (true, true) => ClaimDecision::PossibleAlias { person },
            (false, _) => ClaimDecision::AskIsThisPerson { person },
        };
    }
    let matched = match *evidence {
        VoiceEvidence::Possible { person, .. } => Some(person),
        _ => None,
    };
    if holders.is_empty() {
        return if matched.is_none() {
            ClaimDecision::Enrol
        } else {
            ClaimDecision::AskIsNamed
        };
    }
    if holders.iter().all(|holder| !holder.mature && !holder.owner) {
        let person = matched
            .filter(|person| holders.iter().any(|holder| holder.person == *person))
            .or_else(|| holders.iter().map(|holder| holder.person).min());
        if let Some(person) = person {
            return ClaimDecision::AskIsThisPerson { person };
        }
    }
    ClaimDecision::AskIsNamed
}
