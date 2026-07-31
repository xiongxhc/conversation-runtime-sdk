use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, GenerationId, PrivacyMode, RuntimeEvent,
    SessionId, TurnId, UtteranceId, VoiceSessionEvent, VoiceSessionPolicy,
};

#[test]
fn voice_policy_preserves_explicit_component_locality() {
    let policy = VoiceSessionPolicy::new(
        SessionId::new(7),
        PrivacyMode::LocalOnly,
        200,
        600,
        [
            ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "local-asr",
                ExecutionLocation::Local,
            ),
            ComponentDescriptor::new(
                ComponentKind::LanguageModel,
                "local-language",
                ExecutionLocation::Local,
            ),
        ],
    )
    .unwrap();

    assert_eq!(policy.speech_start_ms(), 200);
    assert_eq!(policy.final_silence_ms(), 600);
    assert!(policy
        .components()
        .iter()
        .all(|item| { item.execution() == ExecutionLocation::Local }));
}

#[test]
fn voice_identity_types_do_not_interchange() {
    let generation = GenerationId::new(9);
    let utterance = UtteranceId::new(9);

    assert_eq!(generation.get(), utterance.get());
    assert_ne!(
        std::any::type_name_of_val(&generation),
        std::any::type_name_of_val(&utterance)
    );
}

#[test]
fn partial_transcript_is_session_scoped_and_nonterminal() {
    let event = VoiceSessionEvent::TranscriptPartial {
        session_id: SessionId::new(1),
        segment_id: 3,
        text: "hel".to_owned(),
    };

    assert!(!event.is_session_terminal());
}

#[test]
fn turn_events_preserve_generation_identity() {
    let event = VoiceSessionEvent::Turn {
        session_id: SessionId::new(1),
        generation_id: GenerationId::new(2),
        event: RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(3),
        },
    };

    assert!(!event.is_session_terminal());
    assert!(matches!(
        event,
        VoiceSessionEvent::Turn {
            generation_id,
            ..
        } if generation_id == GenerationId::new(2)
    ));
}

#[test]
fn voice_policy_rejects_invalid_thresholds_and_components() {
    let component = ComponentDescriptor::new(
        ComponentKind::SpeechRecognition,
        "local-asr",
        ExecutionLocation::Local,
    );

    assert!(VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        200,
        600,
        Vec::new(),
    )
    .is_err());
    assert!(VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        99,
        600,
        [component.clone()],
    )
    .is_err());
    assert!(VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        200,
        3_001,
        [component.clone()],
    )
    .is_err());
    assert!(VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        200,
        600,
        [ComponentDescriptor::new(
            ComponentKind::SpeechRecognition,
            " \t",
            ExecutionLocation::Local,
        )],
    )
    .is_err());
    assert!(VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        100,
        200,
        [component],
    )
    .is_ok());
}
