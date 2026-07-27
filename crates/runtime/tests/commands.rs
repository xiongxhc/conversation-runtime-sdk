use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{DiscardAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
use conversation_protocol::{RuntimeCommand, RuntimeEvent, TurnId};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult, TurnEventStream};

fn minimal_aiff() -> Vec<u8> {
    let mut bytes = Vec::from(&b"FORM"[..]);
    bytes.extend_from_slice(&48_u32.to_be_bytes());
    bytes.extend_from_slice(b"AIFFCOMM");
    bytes.extend_from_slice(&18_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 18]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&9_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&[0x80, 0]);
    bytes
}

#[tokio::test]
async fn executes_typed_start_and_interrupt_commands() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(1);

    let mut events = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: "hello".into(),
        })
        .await
        .unwrap()
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        RuntimeCommandResult::InterruptAccepted => {
            panic!("start command must return a turn event stream")
        }
        _ => panic!("start command returned an unknown result"),
    };

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    assert!(matches!(
        runtime
            .execute(RuntimeCommand::Interrupt { turn_id })
            .await
            .unwrap(),
        RuntimeCommandResult::InterruptAccepted
    ));

    let terminal_events: Vec<_> = drain_events(&mut events)
        .await
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect();
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

async fn drain_events(events: &mut TurnEventStream) -> Vec<RuntimeEvent> {
    let mut observed = Vec::new();
    while let Some(event) = events.recv().await {
        observed.push(event);
    }
    observed
}
