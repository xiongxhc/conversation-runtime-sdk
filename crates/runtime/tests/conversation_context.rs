use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
    MockContinuousAudioOutput, MockStreamingSpeechSynthesizer,
};
use conversation_protocol::{
    ConversationMode, ConversationRole, GenerationId, PersonaProfile, ResponseControls,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, ConversationTurnSource,
    StreamingTurnEventStream, StreamingTurnRuntime,
};
use tokio::sync::{mpsc, Barrier, Notify};
use tokio_util::sync::CancellationToken;

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

#[tokio::test]
async fn text_and_voice_starts_admit_only_one_context_claimant() {
    let context = ConversationContext::new(quality());
    let barrier = Arc::new(Barrier::new(3));

    let text_attempt = {
        let context = context.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            context
                .begin_turn(ConversationTurnSource::Text, "typed")
                .await
        })
    };
    let voice_attempt = {
        let context = context.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            context
                .begin_turn(
                    ConversationTurnSource::Voice {
                        session_id: conversation_protocol::SessionId::new(1),
                    },
                    "spoken",
                )
                .await
        })
    };

    barrier.wait().await;
    let text_result = text_attempt.await.unwrap();
    let voice_result = voice_attempt.await.unwrap();

    let successful_claim = match (text_result, voice_result) {
        (Ok(text), Err(error)) | (Err(error), Ok(text)) => {
            assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
            text
        }
        (Ok(_), Ok(_)) => panic!("both text and voice claims succeeded"),
        (Err(text_error), Err(voice_error)) => {
            panic!("both text and voice claims failed: {text_error:?}, {voice_error:?}")
        }
    };

    assert_eq!(successful_claim.identity().turn_id(), TurnId::new(1));
    assert_eq!(
        successful_claim.identity().generation_id(),
        GenerationId::new(1)
    );
    assert_eq!(
        context.active_turn().await,
        Some(successful_claim.identity())
    );

    context
        .discard_turn(successful_claim.identity(), false)
        .await
        .unwrap();
    assert_eq!(context.active_turn().await, None);

    let next_claim = context
        .begin_turn(ConversationTurnSource::Text, "next")
        .await
        .unwrap();
    assert_eq!(next_claim.identity().turn_id(), TurnId::new(2));
    assert_eq!(next_claim.identity().generation_id(), GenerationId::new(2));
    context
        .discard_turn(next_claim.identity(), false)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_voice_output_is_excluded_from_later_text_history() {
    let context = ConversationContext::new(quality());
    let language = Arc::new(BlockingLanguage::default());
    let runtime = StreamingTurnRuntime::new(
        context.clone(),
        language.clone(),
        Arc::new(MockStreamingSpeechSynthesizer::new([])),
        Arc::new(MockContinuousAudioOutput::new()),
    );
    let mut voice = runtime
        .start_turn(
            ConversationTurnSource::Voice {
                session_id: conversation_protocol::SessionId::new(1),
            },
            "spoken but cancelled",
        )
        .await
        .unwrap();
    let identity = voice.identity();
    language.started.notified().await;
    runtime
        .interrupt(identity.turn_id(), identity.generation_id())
        .await
        .unwrap();
    let events = drain(&mut voice).await;
    assert!(events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnCancelled { .. })));
    assert_eq!(context.active_turn().await, None);

    let text = context
        .begin_turn(ConversationTurnSource::Text, "typed after cancellation")
        .await
        .unwrap();
    assert!(text.resolved().history_messages().is_empty());
    context.discard_turn(text.identity(), false).await.unwrap();
}

#[derive(Default)]
struct BlockingLanguage {
    started: Arc<Notify>,
}

impl GenerationLanguageModel for BlockingLanguage {
    fn stream(
        &self,
        _request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let started = Arc::clone(&self.started);
        tokio::spawn(async move {
            started.notify_one();
            cancellation.cancelled().await;
            drop(sender);
        });
        receiver
    }
}

async fn drain(stream: &mut StreamingTurnEventStream) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    events
}

fn quality() -> ConversationQualityController {
    ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    )
}
