use crate::{RuntimeError, RuntimeErrorKind, RuntimeStage, TurnId};

pub const MAX_HISTORY_MESSAGE_COUNT: usize = 16;
pub const MAX_CONVERSATION_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PersonaLevel(u8);

impl PersonaLevel {
    pub fn new(value: u8) -> Result<Self, RuntimeError> {
        if value > 100 {
            return Err(quality_error("persona level must be within 0..=100"));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersonaProfile {
    warmth: PersonaLevel,
    humor: PersonaLevel,
    teasing: PersonaLevel,
    initiative: PersonaLevel,
    directness: PersonaLevel,
    intimacy: PersonaLevel,
    verbosity: PersonaLevel,
    follow_up_frequency: PersonaLevel,
}

impl PersonaProfile {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        warmth: PersonaLevel,
        humor: PersonaLevel,
        teasing: PersonaLevel,
        initiative: PersonaLevel,
        directness: PersonaLevel,
        intimacy: PersonaLevel,
        verbosity: PersonaLevel,
        follow_up_frequency: PersonaLevel,
    ) -> Self {
        Self {
            warmth,
            humor,
            teasing,
            initiative,
            directness,
            intimacy,
            verbosity,
            follow_up_frequency,
        }
    }

    pub const fn warmth(&self) -> PersonaLevel {
        self.warmth
    }

    pub const fn humor(&self) -> PersonaLevel {
        self.humor
    }

    pub const fn teasing(&self) -> PersonaLevel {
        self.teasing
    }

    pub const fn initiative(&self) -> PersonaLevel {
        self.initiative
    }

    pub const fn directness(&self) -> PersonaLevel {
        self.directness
    }

    pub const fn intimacy(&self) -> PersonaLevel {
        self.intimacy
    }

    pub const fn verbosity(&self) -> PersonaLevel {
        self.verbosity
    }

    pub const fn follow_up_frequency(&self) -> PersonaLevel {
        self.follow_up_frequency
    }

    /// Verbosity buys the spoken budget up front; the quality controller
    /// still reduces it per turn. Above the expansive threshold the curve
    /// steepens into long-form territory (v60 = 52s, v85 = 144s, v100 =
    /// 200s) so a storyteller persona can actually tell a story.
    pub const fn maximum_spoken_seconds(&self) -> u16 {
        let verbosity = self.verbosity.get() as u16;
        let expansive_bonus = verbosity.saturating_sub(60) * 3;
        10 + verbosity * 7 / 10 + expansive_bonus
    }
}

impl Default for PersonaProfile {
    fn default() -> Self {
        Self::new(
            PersonaLevel(80),
            PersonaLevel(60),
            PersonaLevel(40),
            PersonaLevel(35),
            PersonaLevel(80),
            PersonaLevel(30),
            PersonaLevel(20),
            PersonaLevel(25),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConversationMode {
    DirectAnswer,
    Companionship,
    Brainstorming,
    Reflective,
}

impl ConversationMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectAnswer => "direct_answer",
            Self::Companionship => "companionship",
            Self::Brainstorming => "brainstorming",
            Self::Reflective => "reflective",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SpeechPace {
    Measured,
    Natural,
    Brisk,
}

impl SpeechPace {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Natural => "natural",
            Self::Brisk => "brisk",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FollowUpPolicy {
    Never,
    Contextual,
    Allowed,
}

impl FollowUpPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Contextual => "contextual",
            Self::Allowed => "allowed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SilencePolicy {
    AllowWithoutFiller,
}

impl SilencePolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowWithoutFiller => "allow_without_filler",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseControls {
    maximum_spoken_seconds: u16,
    directness: PersonaLevel,
    pace: SpeechPace,
    follow_up_policy: FollowUpPolicy,
    silence_policy: SilencePolicy,
}

impl ResponseControls {
    pub fn new(
        maximum_spoken_seconds: u16,
        directness: PersonaLevel,
        pace: SpeechPace,
        follow_up_policy: FollowUpPolicy,
        silence_policy: SilencePolicy,
    ) -> Result<Self, RuntimeError> {
        if maximum_spoken_seconds == 0 {
            return Err(quality_error(
                "maximum spoken seconds must be greater than zero",
            ));
        }
        Ok(Self {
            maximum_spoken_seconds,
            directness,
            pace,
            follow_up_policy,
            silence_policy,
        })
    }

    pub const fn maximum_spoken_seconds(&self) -> u16 {
        self.maximum_spoken_seconds
    }

    pub const fn directness(&self) -> PersonaLevel {
        self.directness
    }

    pub const fn pace(&self) -> SpeechPace {
        self.pace
    }

    pub const fn follow_up_policy(&self) -> FollowUpPolicy {
        self.follow_up_policy
    }

    pub const fn silence_policy(&self) -> SilencePolicy {
        self.silence_policy
    }
}

impl Default for ResponseControls {
    fn default() -> Self {
        Self::new(
            20,
            PersonaLevel(80),
            SpeechPace::Natural,
            FollowUpPolicy::Contextual,
            SilencePolicy::AllowWithoutFiller,
        )
        .expect("default response controls are valid")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConversationSignal {
    Interrupted,
    ShorterRequested,
    StopExplaining,
    QuestionRejected,
    Hesitation,
    RapidTopicChange,
}

impl ConversationSignal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::ShorterRequested => "shorter_requested",
            Self::StopExplaining => "stop_explaining",
            Self::QuestionRejected => "question_rejected",
            Self::Hesitation => "hesitation",
            Self::RapidTopicChange => "rapid_topic_change",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConversationRole {
    User,
    Assistant,
}

impl ConversationRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationMessage {
    role: ConversationRole,
    text: String,
}

impl ConversationMessage {
    pub fn new(role: ConversationRole, text: impl Into<String>) -> Result<Self, RuntimeError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(quality_error("conversation message must not be empty"));
        }
        if text.len() > MAX_CONVERSATION_MESSAGE_BYTES {
            return Err(quality_error("conversation message exceeds 16 KiB"));
        }
        Ok(Self { role, text })
    }

    pub const fn role(&self) -> ConversationRole {
        self.role
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ContextSource {
    SavedPersona,
    RecentHistory,
    CurrentTurn,
    BargeIn,
    TemporaryCorrection,
}

impl ContextSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SavedPersona => "saved_persona",
            Self::RecentHistory => "recent_history",
            Self::CurrentTurn => "current_turn",
            Self::BargeIn => "barge_in",
            Self::TemporaryCorrection => "temporary_correction",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualityDecision {
    turn_id: TurnId,
    mode: ConversationMode,
    controls: ResponseControls,
    signals: Vec<ConversationSignal>,
    history_message_count: usize,
    context_sources: Vec<ContextSource>,
}

impl QualityDecision {
    pub fn new(
        turn_id: TurnId,
        mode: ConversationMode,
        controls: ResponseControls,
        signals: impl IntoIterator<Item = ConversationSignal>,
        history_message_count: usize,
        context_sources: impl IntoIterator<Item = ContextSource>,
    ) -> Result<Self, RuntimeError> {
        if history_message_count > MAX_HISTORY_MESSAGE_COUNT {
            return Err(quality_error("quality history exceeds eight exchanges"));
        }
        Ok(Self {
            turn_id,
            mode,
            controls,
            signals: unique(signals),
            history_message_count,
            context_sources: unique(context_sources),
        })
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn mode(&self) -> ConversationMode {
        self.mode
    }

    pub const fn controls(&self) -> &ResponseControls {
        &self.controls
    }

    pub fn signals(&self) -> &[ConversationSignal] {
        &self.signals
    }

    pub const fn history_message_count(&self) -> usize {
        self.history_message_count
    }

    pub fn context_sources(&self) -> &[ContextSource] {
        &self.context_sources
    }

    pub fn metric_json(&self) -> String {
        let signals = json_names(self.signals.iter().map(|signal| signal.as_str()));
        let context_sources = json_names(self.context_sources.iter().map(|source| source.as_str()));
        format!(
            concat!(
                "{{\"turn_id\":{},\"mode\":\"{}\",",
                "\"controls\":{{\"maximum_spoken_seconds\":{},\"directness\":{},",
                "\"pace\":\"{}\",\"follow_up_policy\":\"{}\",",
                "\"silence_policy\":\"{}\"}},\"signals\":[{}],",
                "\"history_message_count\":{},\"context_sources\":[{}]}}"
            ),
            self.turn_id.get(),
            self.mode.as_str(),
            self.controls.maximum_spoken_seconds(),
            self.controls.directness().get(),
            self.controls.pace().as_str(),
            self.controls.follow_up_policy().as_str(),
            self.controls.silence_policy().as_str(),
            signals,
            self.history_message_count,
            context_sources,
        )
    }
}

fn unique<T: Copy + PartialEq>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut unique = Vec::new();
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

fn json_names<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    names
        .into_iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(",")
}

fn quality_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::Runtime,
        message,
    )
}
