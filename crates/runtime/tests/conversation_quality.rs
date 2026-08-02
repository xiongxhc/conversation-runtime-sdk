use conversation_protocol::{
    ContextSource, ConversationMode, ConversationRole, ConversationSignal, FollowUpPolicy,
    PersonaLevel, PersonaProfile, ResponseControls, SpeechPace, TurnId,
};
use conversation_runtime::ConversationQualityController;

#[test]
fn short_prompts_resolve_to_short_spoken_controls_in_every_mode() {
    for mode in [
        ConversationMode::DirectAnswer,
        ConversationMode::Companionship,
        ConversationMode::Brainstorming,
        ConversationMode::Reflective,
    ] {
        let mut controller = controller();
        let resolved = controller
            .resolve_turn(TurnId::new(1), "Hello", Some(mode))
            .unwrap()
            .unwrap();

        assert_eq!(resolved.decision().mode(), mode);
        assert_eq!(resolved.decision().controls().maximum_spoken_seconds(), 8);
        assert_eq!(resolved.history_messages().len(), 0);
    }
}

#[test]
fn long_unsegmented_cjk_input_does_not_look_like_one_short_word() {
    let mut controller = controller();
    let resolved = controller
        .resolve_turn(
            TurnId::new(1),
            "这是一个明显超过二十四个汉字而且没有空格的详细说明请求请完整回答",
            None,
        )
        .unwrap()
        .unwrap();

    assert_eq!(resolved.decision().controls().maximum_spoken_seconds(), 20);
}

#[test]
fn explicit_multilingual_signals_resolve_conservative_turn_controls() {
    let scenarios = [
        ("shorter", ConversationSignal::ShorterRequested),
        ("简短一点", ConversationSignal::ShorterRequested),
        ("stop explaining", ConversationSignal::StopExplaining),
        ("别解释了", ConversationSignal::StopExplaining),
        ("um... maybe", ConversationSignal::Hesitation),
        ("换个话题，聊音乐", ConversationSignal::RapidTopicChange),
    ];

    for (input, expected_signal) in scenarios {
        let mut controller = controller();
        let resolved = controller
            .resolve_turn(TurnId::new(1), input, None)
            .unwrap()
            .unwrap();

        assert!(resolved.decision().signals().contains(&expected_signal));
        if matches!(
            expected_signal,
            ConversationSignal::ShorterRequested | ConversationSignal::StopExplaining
        ) {
            assert_eq!(resolved.decision().controls().maximum_spoken_seconds(), 8);
        }
        if expected_signal == ConversationSignal::Hesitation {
            assert_eq!(resolved.decision().controls().pace(), SpeechPace::Measured);
            assert_eq!(
                resolved.decision().controls().follow_up_policy(),
                FollowUpPolicy::Never
            );
        }
    }
}

#[test]
fn rejected_question_is_not_inferred_without_a_completed_question() {
    let mut with_question = controller();
    let first = with_question
        .resolve_turn(TurnId::new(1), "Tell me something", None)
        .unwrap()
        .unwrap();
    assert!(!first
        .decision()
        .signals()
        .contains(&ConversationSignal::QuestionRejected));
    with_question
        .complete_turn(TurnId::new(1), "What would you like to discuss?")
        .unwrap();

    let rejected = with_question
        .resolve_turn(TurnId::new(2), "I'd rather not", None)
        .unwrap()
        .unwrap();
    assert!(rejected
        .decision()
        .signals()
        .contains(&ConversationSignal::QuestionRejected));
    assert_eq!(
        rejected.decision().controls().follow_up_policy(),
        FollowUpPolicy::Never
    );
    assert!(rejected
        .system_guidance()
        .contains("Do not repeat or rephrase the rejected question"));

    let mut fresh = controller();
    let not_rejected = fresh
        .resolve_turn(TurnId::new(1), "I'd rather not", None)
        .unwrap()
        .unwrap();
    assert!(!not_rejected
        .decision()
        .signals()
        .contains(&ConversationSignal::QuestionRejected));
}

#[test]
fn interruption_constrains_exactly_the_next_resolved_turn() {
    let mut controller = controller();
    controller
        .resolve_turn(TurnId::new(1), "Explain the architecture in detail", None)
        .unwrap()
        .unwrap();
    controller.interrupt_turn(TurnId::new(1)).unwrap();

    let constrained = controller
        .resolve_turn(TurnId::new(2), "Continue", None)
        .unwrap()
        .unwrap();
    assert!(constrained
        .decision()
        .signals()
        .contains(&ConversationSignal::Interrupted));
    assert_eq!(
        constrained.decision().controls().maximum_spoken_seconds(),
        8
    );
    assert!(constrained
        .decision()
        .context_sources()
        .contains(&ContextSource::BargeIn));
    controller.discard_turn(TurnId::new(2)).unwrap();

    let following = controller
        .resolve_turn(
            TurnId::new(3),
            "Now continue normally with enough detail",
            None,
        )
        .unwrap()
        .unwrap();
    assert!(!following
        .decision()
        .signals()
        .contains(&ConversationSignal::Interrupted));
}

#[test]
fn temporary_corrections_never_mutate_saved_persona() {
    let persona = PersonaProfile::new(
        level(91),
        level(62),
        level(33),
        level(44),
        level(85),
        level(21),
        level(17),
        level(29),
    );
    let mut controller = ConversationQualityController::new(
        persona.clone(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    );

    controller
        .resolve_turn(TurnId::new(1), "shorter", None)
        .unwrap()
        .unwrap();

    assert_eq!(controller.saved_persona(), &persona);
    assert_eq!(controller.default_controls(), &ResponseControls::default());
    assert_eq!(controller.default_mode(), ConversationMode::DirectAnswer);
}

#[test]
fn only_completed_turns_enter_bounded_history() {
    let mut controller = controller();
    controller
        .resolve_turn(TurnId::new(1), "cancelled user text", None)
        .unwrap()
        .unwrap();
    controller.discard_turn(TurnId::new(1)).unwrap();
    assert!(controller.history_messages().is_empty());

    controller
        .resolve_turn(TurnId::new(2), "completed user text", None)
        .unwrap()
        .unwrap();
    controller
        .complete_turn(TurnId::new(2), "completed assistant text")
        .unwrap();
    let history = controller.history_messages();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role(), ConversationRole::User);
    assert_eq!(history[0].text(), "completed user text");
    assert_eq!(history[1].role(), ConversationRole::Assistant);
    assert_eq!(history[1].text(), "completed assistant text");

    for turn in 3..=11 {
        controller
            .resolve_turn(TurnId::new(turn), format!("user {turn}"), None)
            .unwrap()
            .unwrap();
        controller
            .complete_turn(TurnId::new(turn), format!("assistant {turn}"))
            .unwrap();
    }
    let history = controller.history_messages();
    assert_eq!(history.len(), 16);
    assert!(!history
        .iter()
        .any(|message| message.text() == "completed user text"));
    assert!(controller.history_bytes() <= 16 * 1024);
}

#[test]
fn history_byte_budget_evicts_whole_old_exchanges() {
    let mut controller = controller();
    for turn in 1..=8 {
        controller
            .resolve_turn(
                TurnId::new(turn),
                format!("user-{turn}-{}", "u".repeat(1_100)),
                None,
            )
            .unwrap()
            .unwrap();
        controller
            .complete_turn(
                TurnId::new(turn),
                format!("assistant-{turn}-{}", "a".repeat(1_100)),
            )
            .unwrap();
    }

    let history = controller.history_messages();
    assert!(history.len() < 16);
    assert_eq!(history.len() % 2, 0);
    assert!(controller.history_bytes() <= 16 * 1024);
    assert!(!history
        .iter()
        .any(|message| message.text().starts_with("user-1-")));
}

#[test]
fn relationship_guidance_uses_context_without_scripts_unlocks_or_quotas() {
    let mut controller = controller();
    let first = controller
        .resolve_turn(TurnId::new(1), "Hello", None)
        .unwrap()
        .unwrap();
    assert!(first.relationship_guidance().contains("current turn"));
    controller
        .complete_turn(TurnId::new(1), "Hello, good to meet you.")
        .unwrap();

    let contextual = controller
        .resolve_turn(TurnId::new(2), "Thanks", None)
        .unwrap()
        .unwrap();
    assert!(contextual
        .relationship_guidance()
        .contains("shared context, user reciprocity, pacing, and rapport"));
    for forbidden in [
        "script",
        "unlock",
        "quota",
        "frequency target",
        "special moment",
    ] {
        assert!(!contextual
            .relationship_guidance()
            .to_lowercase()
            .contains(forbidden));
    }
}

#[test]
fn system_guidance_exposes_all_persona_levels_without_intimacy_authorizing_affection() {
    let persona = PersonaProfile::new(
        level(91),
        level(62),
        level(33),
        level(44),
        level(85),
        level(21),
        level(17),
        level(29),
    );
    let mut controller = ConversationQualityController::new(
        persona,
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    );

    let resolved = controller
        .resolve_turn(TurnId::new(1), "Hello", None)
        .unwrap()
        .unwrap();
    let guidance = resolved.system_guidance().to_lowercase();

    for expected in [
        "warmth=91",
        "humor=62",
        "teasing=33",
        "initiative=44",
        "directness=85",
        "intimacy=21",
        "verbosity=17",
        "follow_up_frequency=29",
    ] {
        assert!(guidance.contains(expected));
    }
    assert!(guidance.contains("intimacy never authorizes affection by itself"));
    for forbidden in [
        "script",
        "unlock",
        "quota",
        "frequency target",
        "special moment",
    ] {
        assert!(!guidance.contains(forbidden));
    }
}

#[test]
fn silence_creates_no_pending_turn_or_quality_decision() {
    let mut controller = controller();

    assert!(controller
        .resolve_turn(TurnId::new(1), "  \t\n", None)
        .unwrap()
        .is_none());
    assert!(controller.last_decision().is_none());
    assert!(controller.history_messages().is_empty());
}

fn controller() -> ConversationQualityController {
    ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    )
}

fn level(value: u8) -> PersonaLevel {
    PersonaLevel::new(value).unwrap()
}
