use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFormat, LanguageModel, LanguageModelRequest,
    MockLanguageModel, MockSpeechSynthesizer, SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
};
use conversation_protocol::{RuntimeCommand, RuntimeErrorKind, RuntimeEvent, TurnId};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult, TurnEventStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct CompletionSignallingSpeech {
    completed: Arc<AtomicBool>,
}

struct CleanupAwareSpeech {
    started: Arc<AtomicBool>,
    cleanup_completed: Arc<AtomicBool>,
}

struct CancellationSignallingLanguageModel {
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl LanguageModel for CancellationSignallingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        self.started.store(true, Ordering::Release);
        let cancelled = Arc::clone(&self.cancelled);

        tokio::spawn(async move {
            cancellation.cancelled().await;
            cancelled.store(true, Ordering::Release);
            drop(sender);
        });

        receiver
    }
}

struct InvocationTrackingSpeech {
    invoked: Arc<AtomicBool>,
}

impl SpeechSynthesizer for InvocationTrackingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        self.invoked.store(true, Ordering::Release);
        Box::pin(async { Ok(SynthesizedAudio::new([], AudioFormat::Aiff)) })
    }
}

impl SpeechSynthesizer for CompletionSignallingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.completed.store(true, Ordering::Release);
            Ok(SynthesizedAudio::new([], AudioFormat::Aiff))
        })
    }
}

impl SpeechSynthesizer for CleanupAwareSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            cancellation.cancelled().await;
            tokio::task::yield_now().await;
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new("speech synthesis cancelled"))
        })
    }
}

#[tokio::test]
async fn interruption_reaches_generation_and_skips_synthesis() {
    let generation_started = Arc::new(AtomicBool::new(false));
    let generation_cancelled = Arc::new(AtomicBool::new(false));
    let synthesis_invoked = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(CancellationSignallingLanguageModel {
            started: Arc::clone(&generation_started),
            cancelled: Arc::clone(&generation_cancelled),
        }),
        Arc::new(InvocationTrackingSpeech {
            invoked: Arc::clone(&synthesis_invoked),
        }),
    );
    let turn_id = TurnId::new(6);
    let mut events = start_turn(&runtime, turn_id, "stop generation").await;

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    timeout(Duration::from_secs(1), async {
        while !generation_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generation did not start");
    interrupt(&runtime, turn_id).await.unwrap();

    while events.recv().await.is_some() {}

    timeout(Duration::from_secs(1), async {
        while !generation_cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generation did not observe cancellation");
    assert!(!synthesis_invoked.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn immediate_interruption_preserves_the_started_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(8);
    let mut events = start_turn(&runtime, turn_id, "interrupt immediately").await;

    interrupt(&runtime, turn_id).await.unwrap();

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnCancelled { turn_id })
    );
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
async fn waits_for_speech_cleanup_before_cancellation_completes() {
    let synthesis_started = Arc::new(AtomicBool::new(false));
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(CleanupAwareSpeech {
            started: Arc::clone(&synthesis_started),
            cleanup_completed: Arc::clone(&cleanup_completed),
        }),
    );
    let turn_id = TurnId::new(14);
    let mut events = start_turn(&runtime, turn_id, "cancel during speech").await;

    timeout(Duration::from_secs(1), async {
        while !synthesis_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("speech synthesis did not start");

    interrupt(&runtime, turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert!(cleanup_completed.load(Ordering::Acquire));
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn rejects_a_reused_turn_id() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(13);
    let mut events = start_turn(&runtime, turn_id, "first").await;

    while events.recv().await.is_some() {}

    let error = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: "reused".into(),
        })
        .await
    {
        Ok(_) => panic!("a reused turn identifier must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
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

#[tokio::test]
async fn interruption_finalizes_when_the_event_consumer_is_backpressured() {
    let synthesis_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(vec!["x"; 29])),
        Arc::new(CompletionSignallingSpeech {
            completed: Arc::clone(&synthesis_completed),
        }),
    );
    let turn_id = TurnId::new(12);
    let mut events = start_turn(&runtime, turn_id, "fill the event buffer").await;

    while !synthesis_completed.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

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

async fn start_turn(
    runtime: &ConversationRuntime,
    turn_id: TurnId,
    transcript: &str,
) -> TurnEventStream {
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
