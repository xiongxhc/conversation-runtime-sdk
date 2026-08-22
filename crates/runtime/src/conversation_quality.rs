use std::collections::VecDeque;

use conversation_protocol::{
    ContextSource, ConversationMessage, ConversationMode, ConversationRole, ConversationSignal,
    FollowUpPolicy, PersonaProfile, QualityDecision, ResponseControls, RuntimeError,
    RuntimeErrorKind, RuntimeStage, SilencePolicy, SpeechPace, TurnId,
};

const MAX_HISTORY_EXCHANGES: usize = 8;
const MAX_HISTORY_BYTES: usize = 16 * 1024;
const SHORT_PROMPT_CHARACTERS: usize = 24;
const SHORT_PROMPT_WORDS: usize = 6;
const SHORT_RESPONSE_SECONDS: u16 = 8;
const EXPANSIVE_VERBOSITY_THRESHOLD: u8 = 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConversationQuality {
    saved_persona: PersonaProfile,
    decision: QualityDecision,
    history_messages: Vec<ConversationMessage>,
    system_guidance: String,
    relationship_guidance: String,
}

impl ResolvedConversationQuality {
    pub const fn saved_persona(&self) -> &PersonaProfile {
        &self.saved_persona
    }

    pub const fn decision(&self) -> &QualityDecision {
        &self.decision
    }

    pub fn history_messages(&self) -> &[ConversationMessage] {
        &self.history_messages
    }

    pub fn system_guidance(&self) -> &str {
        &self.system_guidance
    }

    pub fn relationship_guidance(&self) -> &str {
        &self.relationship_guidance
    }
}

#[derive(Clone, Debug)]
pub struct ConversationQualityController {
    saved_persona: PersonaProfile,
    default_controls: ResponseControls,
    default_mode: ConversationMode,
    history: VecDeque<CompletedExchange>,
    history_bytes: usize,
    pending_turn: Option<PendingTurn>,
    interruption_pending: bool,
    last_decision: Option<QualityDecision>,
}

#[derive(Clone, Debug)]
struct CompletedExchange {
    user: ConversationMessage,
    assistant: ConversationMessage,
    bytes: usize,
}

#[derive(Clone, Debug)]
struct PendingTurn {
    turn_id: TurnId,
    user: ConversationMessage,
}

impl ConversationQualityController {
    pub fn new(
        saved_persona: PersonaProfile,
        default_controls: ResponseControls,
        default_mode: ConversationMode,
    ) -> Self {
        Self {
            saved_persona,
            default_controls,
            default_mode,
            history: VecDeque::new(),
            history_bytes: 0,
            pending_turn: None,
            interruption_pending: false,
            last_decision: None,
        }
    }

    pub const fn saved_persona(&self) -> &PersonaProfile {
        &self.saved_persona
    }

    pub const fn default_controls(&self) -> &ResponseControls {
        &self.default_controls
    }

    pub const fn default_mode(&self) -> ConversationMode {
        self.default_mode
    }

    pub const fn last_decision(&self) -> Option<&QualityDecision> {
        self.last_decision.as_ref()
    }

    pub const fn history_bytes(&self) -> usize {
        self.history_bytes
    }

    pub fn history_messages(&self) -> Vec<ConversationMessage> {
        self.history
            .iter()
            .flat_map(|exchange| [exchange.user.clone(), exchange.assistant.clone()])
            .collect()
    }

    /// Replaces the saved persona, default mode, and default response
    /// controls used by future turns. An actual change starts a fresh
    /// in-session conversation context. Rejected while a turn is pending, the
    /// same guard `resolve_turn` enforces, so a mutation can never land
    /// mid-turn.
    pub fn set_persona(
        &mut self,
        persona: PersonaProfile,
        mode: ConversationMode,
    ) -> Result<(), RuntimeError> {
        if self.pending_turn.is_some() {
            return Err(state_error("a quality-controlled turn is still pending"));
        }
        let default_controls = ResponseControls::new(
            persona.maximum_spoken_seconds(),
            persona.directness(),
            SpeechPace::Natural,
            FollowUpPolicy::Contextual,
            SilencePolicy::AllowWithoutFiller,
        )?;
        let changed = self.saved_persona != persona || self.default_mode != mode;
        self.saved_persona = persona;
        self.default_mode = mode;
        self.default_controls = default_controls;
        if changed {
            self.clear_history();
        }
        Ok(())
    }

    pub fn resolve_turn(
        &mut self,
        turn_id: TurnId,
        transcript: impl Into<String>,
        mode: Option<ConversationMode>,
    ) -> Result<Option<ResolvedConversationQuality>, RuntimeError> {
        if self.pending_turn.is_some() {
            return Err(state_error("a quality-controlled turn is still pending"));
        }

        let transcript = transcript.into();
        if transcript.trim().is_empty() {
            return Ok(None);
        }
        let user = ConversationMessage::new(ConversationRole::User, transcript)?;
        let mut signals =
            detect_signals(user.text(), self.previous_assistant_ended_with_question());
        if self.interruption_pending {
            signals.insert(0, ConversationSignal::Interrupted);
            self.interruption_pending = false;
        }
        if signals.contains(&ConversationSignal::RapidTopicChange) {
            self.clear_history();
        }

        let controls = self.resolve_controls(user.text(), &signals)?;
        let mode = mode.unwrap_or(self.default_mode);
        let history_messages = self.history_messages();
        let context_sources = context_sources(&signals, history_messages.is_empty());
        let decision = QualityDecision::new(
            turn_id,
            mode,
            controls,
            signals,
            history_messages.len(),
            context_sources,
        )?;
        let relationship_guidance = relationship_guidance(history_messages.is_empty());
        let system_guidance =
            system_guidance(&self.saved_persona, &decision, relationship_guidance);

        self.pending_turn = Some(PendingTurn { turn_id, user });
        self.last_decision = Some(decision.clone());

        Ok(Some(ResolvedConversationQuality {
            saved_persona: self.saved_persona.clone(),
            decision,
            history_messages,
            system_guidance,
            relationship_guidance: relationship_guidance.to_owned(),
        }))
    }

    pub fn complete_turn(
        &mut self,
        turn_id: TurnId,
        assistant_output: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        let pending = self.take_pending(turn_id)?;
        let assistant_output = assistant_output.into();
        if assistant_output.len() > MAX_HISTORY_BYTES {
            return Ok(());
        }
        let assistant = ConversationMessage::new(ConversationRole::Assistant, assistant_output)?;
        let bytes = pending.user.text().len() + assistant.text().len();
        if bytes > MAX_HISTORY_BYTES {
            return Ok(());
        }

        self.history.push_back(CompletedExchange {
            user: pending.user,
            assistant,
            bytes,
        });
        self.history_bytes += bytes;
        self.enforce_history_bounds();
        Ok(())
    }

    pub fn discard_turn(&mut self, turn_id: TurnId) -> Result<(), RuntimeError> {
        self.take_pending(turn_id).map(|_| ())
    }

    pub fn interrupt_turn(&mut self, turn_id: TurnId) -> Result<(), RuntimeError> {
        self.take_pending(turn_id)?;
        self.interruption_pending = true;
        Ok(())
    }

    fn previous_assistant_ended_with_question(&self) -> bool {
        self.history
            .back()
            .is_some_and(|exchange| exchange.assistant.text().trim_end().ends_with(['?', '？']))
    }

    fn resolve_controls(
        &self,
        transcript: &str,
        signals: &[ConversationSignal],
    ) -> Result<ResponseControls, RuntimeError> {
        let mut maximum_spoken_seconds = self.default_controls.maximum_spoken_seconds();
        // An expansive persona has explicitly chosen long-form speech, so a
        // terse prompt or an incidental interruption reduces the budget
        // proportionally instead of forcing the absolute short cap. An
        // explicit request for brevity always wins.
        let expansive = self.saved_persona.verbosity().get() >= EXPANSIVE_VERBOSITY_THRESHOLD;
        let reduced_budget = if expansive {
            (self.default_controls.maximum_spoken_seconds() / 2).max(SHORT_RESPONSE_SECONDS)
        } else {
            SHORT_RESPONSE_SECONDS
        };
        if is_short_prompt(transcript) {
            maximum_spoken_seconds = maximum_spoken_seconds.min(reduced_budget);
        }
        let explicit_brevity = signals.iter().any(|signal| {
            matches!(
                signal,
                ConversationSignal::ShorterRequested | ConversationSignal::StopExplaining
            )
        });
        if explicit_brevity {
            maximum_spoken_seconds = maximum_spoken_seconds
                .min(SHORT_RESPONSE_SECONDS)
                .min((self.default_controls.maximum_spoken_seconds() / 2).max(1));
        } else if signals.contains(&ConversationSignal::Interrupted) {
            maximum_spoken_seconds = maximum_spoken_seconds
                .min(reduced_budget)
                .min((self.default_controls.maximum_spoken_seconds() / 2).max(1));
        }

        let hesitant = signals.contains(&ConversationSignal::Hesitation);
        let rejected = signals.contains(&ConversationSignal::QuestionRejected);
        ResponseControls::new(
            maximum_spoken_seconds,
            self.default_controls.directness(),
            if hesitant {
                SpeechPace::Measured
            } else {
                self.default_controls.pace()
            },
            if hesitant || rejected {
                FollowUpPolicy::Never
            } else {
                self.default_controls.follow_up_policy()
            },
            self.default_controls.silence_policy(),
        )
    }

    fn take_pending(&mut self, turn_id: TurnId) -> Result<PendingTurn, RuntimeError> {
        match self.pending_turn.take() {
            Some(pending) if pending.turn_id == turn_id => Ok(pending),
            Some(pending) => {
                let active_turn_id = pending.turn_id;
                self.pending_turn = Some(pending);
                Err(state_error(format!(
                    "quality turn {active_turn_id} is pending, not turn {turn_id}"
                )))
            }
            None => Err(state_error("there is no pending quality-controlled turn")),
        }
    }

    fn enforce_history_bounds(&mut self) {
        while self.history.len() > MAX_HISTORY_EXCHANGES || self.history_bytes > MAX_HISTORY_BYTES {
            let removed = self
                .history
                .pop_front()
                .expect("history bounds require a retained exchange");
            self.history_bytes -= removed.bytes;
        }
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.history_bytes = 0;
    }
}

fn detect_signals(
    transcript: &str,
    previous_assistant_asked_question: bool,
) -> Vec<ConversationSignal> {
    let normalized = normalize(transcript);
    let mut signals = Vec::new();

    if contains_any(
        &normalized,
        &["shorter", "briefly", "keep it short", "简短一点", "说短点"],
    ) {
        signals.push(ConversationSignal::ShorterRequested);
    }
    if contains_any(
        &normalized,
        &[
            "stop explaining",
            "don't explain",
            "do not explain",
            "no more explanation",
            "别解释了",
            "不要解释",
        ],
    ) {
        signals.push(ConversationSignal::StopExplaining);
    }
    if previous_assistant_asked_question && is_explicit_question_rejection(&normalized) {
        signals.push(ConversationSignal::QuestionRejected);
    }
    if normalized.contains("...")
        || normalized.contains('…')
        || contains_token(&normalized, &["um", "uh", "hmm", "呃", "嗯"])
    {
        signals.push(ConversationSignal::Hesitation);
    }
    if contains_any(
        &normalized,
        &[
            "different topic",
            "change the subject",
            "switch topics",
            "new topic",
            "talk about something else",
            "let's move on",
            "换个话题",
            "换一个话题",
            "不聊这个",
            "先不说这个",
            "说点别的",
            "聊点别的",
        ],
    ) {
        signals.push(ConversationSignal::RapidTopicChange);
    }

    signals
}

fn normalize(input: &str) -> String {
    input
        .trim()
        .trim_matches(|character: char| {
            matches!(
                character,
                '.' | ',' | '!' | '?' | ';' | ':' | '。' | '，' | '！' | '？' | '；' | '：'
            )
        })
        .to_lowercase()
}

fn contains_any(input: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| input.contains(phrase))
}

fn contains_token(input: &str, tokens: &[&str]) -> bool {
    input
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| tokens.contains(&word))
}

fn is_explicit_question_rejection(input: &str) -> bool {
    matches!(
        input,
        "no" | "no thanks"
            | "i'd rather not"
            | "i would rather not"
            | "don't ask me that"
            | "do not ask me that"
            | "not answering that"
            | "不想回答"
            | "别问了"
    )
}

fn is_short_prompt(input: &str) -> bool {
    let word_count = input.split_whitespace().count();
    input.chars().count() <= SHORT_PROMPT_CHARACTERS
        || (word_count > 1 && word_count <= SHORT_PROMPT_WORDS)
}

fn context_sources(signals: &[ConversationSignal], history_is_empty: bool) -> Vec<ContextSource> {
    let mut sources = vec![ContextSource::SavedPersona];
    if !history_is_empty {
        sources.push(ContextSource::RecentHistory);
    }
    sources.push(ContextSource::CurrentTurn);
    if signals.contains(&ConversationSignal::Interrupted) {
        sources.push(ContextSource::BargeIn);
    }
    if signals.iter().any(|signal| {
        matches!(
            signal,
            ConversationSignal::ShorterRequested
                | ConversationSignal::StopExplaining
                | ConversationSignal::QuestionRejected
                | ConversationSignal::RapidTopicChange
        )
    }) {
        sources.push(ContextSource::TemporaryCorrection);
    }
    sources
}

fn relationship_guidance(history_is_empty: bool) -> &'static str {
    if history_is_empty {
        "Use only the current turn to establish tone; do not imply shared history or earned familiarity."
    } else {
        "Let warmth and familiarity reflect only the supplied shared context, user reciprocity, pacing, and rapport; do not manufacture closeness."
    }
}

fn system_guidance(
    persona: &PersonaProfile,
    decision: &QualityDecision,
    relationship: &str,
) -> String {
    let controls = decision.controls();
    let mut guidance = format!(
        concat!(
            "Saved persona levels (0-100): warmth={}, humor={}, teasing={}, initiative={}, ",
            "directness={}, intimacy={}, verbosity={}, follow_up_frequency={}. ",
            "Intimacy may shape tone only when supported by shared context and reciprocity; ",
            "intimacy never authorizes affection by itself. ",
            "Use {} mode. Keep spoken output within {} seconds. Use {} pace and {} directness. ",
            "Follow-up policy is {}. Silence may remain without filler. {}"
        ),
        persona.warmth().get(),
        persona.humor().get(),
        persona.teasing().get(),
        persona.initiative().get(),
        persona.directness().get(),
        persona.intimacy().get(),
        persona.verbosity().get(),
        persona.follow_up_frequency().get(),
        decision.mode().as_str(),
        controls.maximum_spoken_seconds(),
        controls.pace().as_str(),
        controls.directness().get(),
        controls.follow_up_policy().as_str(),
        relationship,
    );
    if decision
        .signals()
        .contains(&ConversationSignal::QuestionRejected)
    {
        guidance.push_str(" Do not repeat or rephrase the rejected question.");
    }
    if decision
        .signals()
        .contains(&ConversationSignal::RapidTopicChange)
    {
        guidance.push_str(" Follow the new topic without returning to the previous one.");
    }
    if decision.signals().contains(&ConversationSignal::Hesitation) {
        guidance.push_str(" Leave the user room to continue without pressure.");
    }
    if decision
        .signals()
        .contains(&ConversationSignal::Interrupted)
    {
        guidance.push_str(" Resume only what the user currently requests.");
    }
    guidance
}

fn state_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}
