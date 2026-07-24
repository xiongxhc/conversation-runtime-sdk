use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, LanguageModel, LanguageModelRequest, MockLanguageModel, MockSpeechSynthesizer,
};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct FailingLanguageModel;

impl LanguageModel for FailingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Err(AdapterError::new("language model unavailable")))
                .await;
        });
        receiver
    }
}

#[tokio::test]
async fn emits_an_ordered_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["hello", " there"])),
        Arc::new(MockSpeechSynthesizer::new([1, 2, 3])),
    );
    let turn_id = TurnId::new(1);
    let mut events = start_turn(&runtime, turn_id, "hi").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
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
    assert_eq!(
        observed.iter().filter(|event| event.is_terminal()).count(),
        1
    );
}

#[tokio::test]
async fn reports_language_model_failure_as_the_only_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(FailingLanguageModel),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(2);
    let mut events = start_turn(&runtime, turn_id, "fail").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    let terminal_events: Vec<_> = observed
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect();
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::LanguageModel,
                "language model unavailable",
            ),
        }]
    );
}

#[tokio::test]
async fn cancels_during_speech_synthesis() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::delayed([1], Duration::from_secs(5))),
    );
    let turn_id = TurnId::new(3);
    let mut events = start_turn(&runtime, turn_id, "speak").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        let speech_started = matches!(event, RuntimeEvent::SpeechStarted { .. });
        observed.push(event);
        if speech_started {
            interrupt(&runtime, turn_id).await;
        }
    }

    let terminal_events: Vec<_> = observed
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect();
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn reuses_runtime_after_a_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );

    for turn_number in [10, 11] {
        let turn_id = TurnId::new(turn_number);
        let mut events = start_turn(&runtime, turn_id, "again").await;
        let mut terminal_events = Vec::new();

        while let Some(event) = events.recv().await {
            if event.is_terminal() {
                terminal_events.push(event);
            }
        }

        assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCompleted { turn_id }]
        );
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

async fn interrupt(runtime: &ConversationRuntime, turn_id: TurnId) {
    assert!(matches!(
        runtime
            .execute(RuntimeCommand::Interrupt { turn_id })
            .await
            .unwrap(),
        RuntimeCommandResult::InterruptAccepted
    ));
}
