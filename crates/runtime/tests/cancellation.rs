use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{MockLanguageModel, MockSpeechSynthesizer};
use conversation_protocol::{RuntimeErrorKind, RuntimeEvent, TurnId};
use conversation_runtime::ConversationRuntime;

#[tokio::test]
async fn interruption_emits_one_cancelled_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(7);
    let mut events = runtime.start_turn(turn_id, "stop").await.unwrap();

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    runtime.interrupt(turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
            break;
        }
    }

    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn rejects_a_second_active_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let first_turn = TurnId::new(1);
    let second_turn = TurnId::new(2);
    let _events = runtime.start_turn(first_turn, "first").await.unwrap();

    let error = match runtime.start_turn(second_turn, "second").await {
        Ok(_) => panic!("a second active turn must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind, RuntimeErrorKind::InvalidState);
    runtime.interrupt(first_turn).await.unwrap();
}
