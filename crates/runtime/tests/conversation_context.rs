use conversation_protocol::{
    ConversationMode, ConversationRole, GenerationId, PersonaProfile, ResponseControls,
    RuntimeErrorKind, RuntimeStage, TurnId,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, ConversationTurnSource,
};

#[tokio::test]
async fn context_allocates_monotonic_ids_and_rejects_a_second_active_turn() {
    let context = ConversationContext::new(quality());

    let first = context
        .begin_turn(ConversationTurnSource::Text, "first")
        .await
        .unwrap();
    assert_eq!(first.identity().turn_id(), TurnId::new(1));
    assert_eq!(first.identity().generation_id(), GenerationId::new(1));
    assert!(context
        .begin_turn(ConversationTurnSource::Text, "second")
        .await
        .is_err());

    context
        .complete_turn(first.identity(), "answer")
        .await
        .unwrap();

    let second = context
        .begin_turn(ConversationTurnSource::Text, "second")
        .await
        .unwrap();
    assert_eq!(second.identity().turn_id(), TurnId::new(2));
    assert_eq!(second.identity().generation_id(), GenerationId::new(2));
}

#[tokio::test]
async fn discarded_turns_are_excluded_from_completed_history() {
    let context = ConversationContext::new(quality());

    let failed = context
        .begin_turn(ConversationTurnSource::Text, "failed user text")
        .await
        .unwrap();
    context
        .discard_turn(failed.identity(), false)
        .await
        .unwrap();

    let cancelled = context
        .begin_turn(ConversationTurnSource::Text, "cancelled user text")
        .await
        .unwrap();
    context
        .discard_turn(cancelled.identity(), true)
        .await
        .unwrap();

    let completed = context
        .begin_turn(ConversationTurnSource::Text, "completed user text")
        .await
        .unwrap();
    context
        .complete_turn(completed.identity(), "completed assistant text")
        .await
        .unwrap();

    let next = context
        .begin_turn(ConversationTurnSource::Text, "next user text")
        .await
        .unwrap();
    let history = next.resolved().history_messages();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role(), ConversationRole::User);
    assert_eq!(history[0].text(), "completed user text");
    assert_eq!(history[1].role(), ConversationRole::Assistant);
    assert_eq!(history[1].text(), "completed assistant text");
}

#[tokio::test]
async fn sequence_overflow_returns_a_typed_error_without_reserving_a_turn() {
    let context = ConversationContext::new(quality()).with_test_sequence_for_test(u64::MAX);

    let error = context
        .begin_turn(ConversationTurnSource::Text, "overflow")
        .await
        .err()
        .expect("sequence overflow should reject turn allocation");

    assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
    assert_eq!(error.stage(), RuntimeStage::Runtime);
    assert_eq!(context.active_turn().await, None);
}

#[tokio::test]
async fn failed_completion_releases_the_context_for_a_new_turn() {
    let context = ConversationContext::new(quality());
    let first = context
        .begin_turn(ConversationTurnSource::Text, "first")
        .await
        .unwrap();

    assert!(context.complete_turn(first.identity(), "").await.is_err());
    assert_eq!(context.active_turn().await, None);

    let second = context
        .begin_turn(ConversationTurnSource::Text, "second")
        .await
        .unwrap();
    assert_eq!(second.identity().turn_id(), TurnId::new(2));
}

fn quality() -> ConversationQualityController {
    ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    )
}
