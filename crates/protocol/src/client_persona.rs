use serde::{Deserialize, Serialize};

use crate::{ClientWireError, ConversationMode, PersonaLevel, PersonaProfile};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPersonaState {
    pub mode: String,
    pub warmth: u8,
    pub humor: u8,
    pub teasing: u8,
    pub initiative: u8,
    pub directness: u8,
    pub intimacy: u8,
    pub verbosity: u8,
    pub follow_up_frequency: u8,
}

impl ClientPersonaState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: impl Into<String>,
        warmth: u8,
        humor: u8,
        teasing: u8,
        initiative: u8,
        directness: u8,
        intimacy: u8,
        verbosity: u8,
        follow_up_frequency: u8,
    ) -> Result<Self, ClientWireError> {
        let state = Self {
            mode: mode.into(),
            warmth,
            humor,
            teasing,
            initiative,
            directness,
            intimacy,
            verbosity,
            follow_up_frequency,
        };
        validate_client_persona_state(&state)?;
        Ok(state)
    }

    pub fn from_profile(profile: &PersonaProfile, mode: ConversationMode) -> Self {
        Self {
            mode: mode.as_str().to_owned(),
            warmth: profile.warmth().get(),
            humor: profile.humor().get(),
            teasing: profile.teasing().get(),
            initiative: profile.initiative().get(),
            directness: profile.directness().get(),
            intimacy: profile.intimacy().get(),
            verbosity: profile.verbosity().get(),
            follow_up_frequency: profile.follow_up_frequency().get(),
        }
    }

    pub fn to_profile(&self) -> Result<(PersonaProfile, ConversationMode), ClientWireError> {
        validate_client_persona_state(self)?;
        let mode = conversation_mode_from_wire_name(&self.mode)
            .expect("mode was validated as a known wire name");
        let level =
            |value: u8| PersonaLevel::new(value).expect("level was validated as within 0..=100");
        let profile = PersonaProfile::new(
            level(self.warmth),
            level(self.humor),
            level(self.teasing),
            level(self.initiative),
            level(self.directness),
            level(self.intimacy),
            level(self.verbosity),
            level(self.follow_up_frequency),
        );
        Ok((profile, mode))
    }
}

pub(crate) fn validate_client_persona_state(
    state: &ClientPersonaState,
) -> Result<(), ClientWireError> {
    if conversation_mode_from_wire_name(&state.mode).is_none() {
        return Err(ClientWireError::InvalidPersonaState);
    }
    for level in [
        state.warmth,
        state.humor,
        state.teasing,
        state.initiative,
        state.directness,
        state.intimacy,
        state.verbosity,
        state.follow_up_frequency,
    ] {
        if level > 100 {
            return Err(ClientWireError::InvalidPersonaState);
        }
    }
    Ok(())
}

fn conversation_mode_from_wire_name(name: &str) -> Option<ConversationMode> {
    match name {
        "direct_answer" => Some(ConversationMode::DirectAnswer),
        "companionship" => Some(ConversationMode::Companionship),
        "brainstorming" => Some(ConversationMode::Brainstorming),
        "reflective" => Some(ConversationMode::Reflective),
        _ => None,
    }
}
