use conversation_model_adapters::{
    tagged_user_text, LanguageModelInput, MAX_RUNTIME_GUIDANCE_BYTES,
};
use conversation_protocol::{
    ContextSource, ConversationMode, QualityDecision, ResponseControls, SpeakerTag, TurnId,
};

fn input(guidance: &str) -> LanguageModelInput {
    LanguageModelInput::with_quality(
        "current request",
        [],
        QualityDecision::new(
            TurnId::new(1),
            ConversationMode::DirectAnswer,
            ResponseControls::default(),
            [],
            0,
            [ContextSource::SavedPersona],
        )
        .unwrap(),
        guidance,
    )
    .unwrap()
}

#[test]
fn identity_guidance_is_appended_after_a_blank_line() {
    let input = input("Persona guidance.")
        .with_identity_guidance("Identity guidance.")
        .unwrap();
    assert_eq!(
        input.runtime_guidance(),
        Some("Persona guidance.\n\nIdentity guidance.")
    );
}

#[test]
fn identity_guidance_cannot_push_guidance_past_its_limit() {
    let persona = "p".repeat(MAX_RUNTIME_GUIDANCE_BYTES - 10);
    assert!(input(&persona)
        .with_identity_guidance("twenty bytes of text")
        .is_err());
}

#[test]
fn the_speaker_and_guidance_survive_adding_memory() {
    let tag = SpeakerTag::person(Some("Jack"), true);
    let input = input("Persona guidance.")
        .with_speaker(tag.clone())
        .with_identity_guidance("Identity guidance.")
        .unwrap()
        .with_memory_items([])
        .unwrap();
    assert_eq!(input.speaker(), Some(&tag));
    assert_eq!(
        input.runtime_guidance(),
        Some("Persona guidance.\n\nIdentity guidance.")
    );
}

#[test]
fn tagged_user_text_prefixes_only_tagged_text() {
    let tag = SpeakerTag::person(Some("Jack"), false);
    assert_eq!(tagged_user_text("hi", Some(&tag)), "[Jack?] hi");
    assert_eq!(tagged_user_text("hi", None), "hi");
}
