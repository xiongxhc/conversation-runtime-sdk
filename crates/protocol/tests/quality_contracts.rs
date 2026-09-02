use conversation_protocol::{
    ContextSource, ConversationMessage, ConversationMode, ConversationRole, ConversationSignal,
    FollowUpPolicy, PersonaLevel, PersonaProfile, QualityDecision, ResponseControls,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, SilencePolicy, SpeechPace, TurnId,
    MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_BYTES, MAX_HISTORY_MESSAGE_COUNT,
};

#[test]
fn persona_levels_and_response_controls_enforce_public_bounds() {
    assert_eq!(PersonaLevel::new(0).unwrap().get(), 0);
    assert_eq!(PersonaLevel::new(100).unwrap().get(), 100);

    let error = PersonaLevel::new(101).unwrap_err();
    assert_eq!(error.kind(), RuntimeErrorKind::Configuration);
    assert_eq!(error.stage(), RuntimeStage::Runtime);
    assert!(ResponseControls::new(
        0,
        PersonaLevel::new(80).unwrap(),
        SpeechPace::Natural,
        FollowUpPolicy::Contextual,
        SilencePolicy::AllowWithoutFiller,
    )
    .is_err());
}

#[test]
fn maximum_spoken_seconds_follows_the_expansive_verbosity_curve() {
    let profile = |verbosity: u8| {
        PersonaProfile::new(
            level(80),
            level(60),
            level(40),
            level(35),
            level(80),
            level(30),
            level(verbosity),
            level(25),
        )
    };

    assert_eq!(profile(20).maximum_spoken_seconds(), 24);
    assert_eq!(profile(60).maximum_spoken_seconds(), 52);
    assert_eq!(profile(85).maximum_spoken_seconds(), 144);
    assert_eq!(profile(100).maximum_spoken_seconds(), 200);
}

#[test]
fn persona_and_response_defaults_are_explicit_and_inspectable() {
    let persona = PersonaProfile::default();
    let controls = ResponseControls::default();

    assert_eq!(persona.warmth().get(), 80);
    assert_eq!(persona.humor().get(), 60);
    assert_eq!(persona.teasing().get(), 40);
    assert_eq!(persona.initiative().get(), 35);
    assert_eq!(persona.directness().get(), 80);
    assert_eq!(persona.intimacy().get(), 30);
    assert_eq!(persona.verbosity().get(), 20);
    assert_eq!(persona.follow_up_frequency().get(), 25);
    assert_eq!(controls.maximum_spoken_seconds(), 20);
    assert_eq!(controls.directness().get(), 80);
    assert_eq!(controls.pace(), SpeechPace::Natural);
    assert_eq!(controls.follow_up_policy(), FollowUpPolicy::Contextual);
    assert_eq!(controls.silence_policy(), SilencePolicy::AllowWithoutFiller);
}

#[test]
fn all_modes_signals_and_context_messages_have_typed_representations() {
    assert_eq!(
        [
            ConversationMode::DirectAnswer,
            ConversationMode::Companionship,
            ConversationMode::Brainstorming,
            ConversationMode::Reflective,
        ]
        .len(),
        4
    );
    assert_eq!(
        [
            ConversationSignal::Interrupted,
            ConversationSignal::ShorterRequested,
            ConversationSignal::StopExplaining,
            ConversationSignal::QuestionRejected,
            ConversationSignal::Hesitation,
            ConversationSignal::RapidTopicChange,
        ]
        .len(),
        6
    );

    let user = ConversationMessage::new(ConversationRole::User, "hello").unwrap();
    let assistant = ConversationMessage::new(ConversationRole::Assistant, "hi").unwrap();
    assert_eq!(user.role(), ConversationRole::User);
    assert_eq!(user.text(), "hello");
    assert_eq!(assistant.role(), ConversationRole::Assistant);
    assert!(ConversationMessage::new(ConversationRole::User, " \t\n").is_err());
    assert!(ConversationMessage::new(
        ConversationRole::User,
        "x".repeat(MAX_CONVERSATION_MESSAGE_BYTES),
    )
    .is_ok());
    assert!(ConversationMessage::new(ConversationRole::User, "x".repeat(16 * 1024 + 1)).is_err());
}

#[test]
fn history_envelope_constants_distinguish_message_and_history_limits() {
    assert_eq!(MAX_HISTORY_MESSAGE_COUNT, 32);
    assert_eq!(MAX_HISTORY_BYTES, 32 * 1024);
    assert_eq!(MAX_CONVERSATION_MESSAGE_BYTES, 16 * 1024);
}

#[test]
fn quality_event_serialization_is_content_free_and_nonterminal() {
    let turn_id = TurnId::new(7);
    let decision = QualityDecision::new(
        turn_id,
        ConversationMode::Reflective,
        ResponseControls::new(
            8,
            PersonaLevel::new(90).unwrap(),
            SpeechPace::Measured,
            FollowUpPolicy::Never,
            SilencePolicy::AllowWithoutFiller,
        )
        .unwrap(),
        [
            ConversationSignal::ShorterRequested,
            ConversationSignal::Hesitation,
        ],
        4,
        [
            ContextSource::SavedPersona,
            ContextSource::RecentHistory,
            ContextSource::CurrentTurn,
            ContextSource::TemporaryCorrection,
        ],
    )
    .unwrap();
    let event = RuntimeEvent::QualityResolved { decision };

    let json = event.quality_metric_json().unwrap();
    assert_eq!(event.turn_id(), turn_id);
    assert!(!event.is_terminal());
    assert!(json.contains("\"event\":\"quality_resolved\""));
    assert!(json.contains("\"mode\":\"reflective\""));
    assert!(json.contains("\"maximum_spoken_seconds\":8"));
    assert!(json.contains("\"history_message_count\":4"));
    for forbidden in [
        "prompt",
        "transcript",
        "generated_text",
        "private user words",
    ] {
        assert!(!json.contains(forbidden));
    }
}

fn level(value: u8) -> PersonaLevel {
    PersonaLevel::new(value).unwrap()
}
