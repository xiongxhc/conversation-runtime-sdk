use std::sync::Arc;

use conversation_model_adapters::{MockLanguageModel, MockSpeechSynthesizer};
use conversation_protocol::{RuntimeEvent, TurnId};
use conversation_runtime::ConversationRuntime;

#[tokio::test]
async fn emits_an_ordered_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["hello", " there"])),
        Arc::new(MockSpeechSynthesizer::new([1, 2, 3])),
    );
    let turn_id = TurnId::new(1);
    let mut events = runtime.start_turn(turn_id, "hi").await.unwrap();
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        let terminal = event.is_terminal();
        observed.push(event);
        if terminal {
            break;
        }
    }

    assert_eq!(
        observed,
        vec![
            RuntimeEvent::TurnStarted { turn_id },
            RuntimeEvent::TranscriptFinal {
                turn_id,
                text: "hi".into(),
            },
            RuntimeEvent::TextDelta {
                turn_id,
                delta: "hello".into(),
            },
            RuntimeEvent::TextDelta {
                turn_id,
                delta: " there".into(),
            },
            RuntimeEvent::SpeechStarted { turn_id },
            RuntimeEvent::SpeechCompleted { turn_id },
            RuntimeEvent::TurnCompleted { turn_id },
        ]
    );
}
