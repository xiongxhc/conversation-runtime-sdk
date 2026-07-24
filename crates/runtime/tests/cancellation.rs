use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterFuture, MockLanguageModel, MockSpeechSynthesizer, SpeechRequest, SpeechSynthesizer,
};
use conversation_protocol::{RuntimeCommand, RuntimeErrorKind, RuntimeEvent, TurnId};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult};
use tokio_util::sync::CancellationToken;

struct CompletionSignallingSpeech {
    completed: Arc<AtomicBool>,
}

impl SpeechSynthesizer for CompletionSignallingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.completed.store(true, Ordering::Release);
            Ok(Vec::new())
        })
    }
}

#[tokio::test]
async fn interruption_emits_one_cancelled_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(7);
    let mut events = start_turn(&runtime, turn_id, "stop").await;

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    interrupt(&runtime, turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
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
    let _events = start_turn(&runtime, first_turn, "first").await;

    let error = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id: second_turn,
            transcript: "second".into(),
        })
        .await
    {
        Ok(_) => panic!("a second active turn must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
    interrupt(&runtime, first_turn).await.unwrap();
}

#[tokio::test]
async fn interruption_result_matches_the_terminal_event_at_synthesis_boundary() {
    let synthesis_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(vec!["x"; 29])),
        Arc::new(CompletionSignallingSpeech {
            completed: Arc::clone(&synthesis_completed),
        }),
    );
    let turn_id = TurnId::new(9);
    let mut events = start_turn(&runtime, turn_id, "fill the event buffer").await;

    while !synthesis_completed.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    let interruption = interrupt(&runtime, turn_id).await;

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    match interruption {
        Ok(()) => assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCancelled { turn_id }]
        ),
        Err(_) => assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCompleted { turn_id }]
        ),
    }
}

async fn start_turn(
    runtime: &ConversationRuntime,
    turn_id: TurnId,
    transcript: &str,
) -> tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent> {
    match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: transcript.into(),
        })
        .await
        .unwrap()
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        RuntimeCommandResult::InterruptAccepted => {
            panic!("start command must return a turn event stream")
        }
        _ => panic!("start command returned an unknown result"),
    }
}

async fn interrupt(
    runtime: &ConversationRuntime,
    turn_id: TurnId,
) -> Result<(), conversation_protocol::RuntimeError> {
    match runtime
        .execute(RuntimeCommand::Interrupt { turn_id })
        .await?
    {
        RuntimeCommandResult::InterruptAccepted => Ok(()),
        RuntimeCommandResult::TurnStarted { .. } => {
            panic!("interrupt command must not return a turn event stream")
        }
        _ => panic!("interrupt command returned an unknown result"),
    }
}
