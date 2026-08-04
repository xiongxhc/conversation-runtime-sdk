use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use conversation_memory::{
    MemoryClock, MemoryContextProvider, MemoryProviderFuture, MemoryStoreError, MemoryStoreResult,
    SqliteMemoryContextProvider, SqliteMemoryStore,
};
use conversation_model_adapters::{
    AdapterError, GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
    MockGenerationLanguageModel,
};
use conversation_protocol::{
    ConversationMode, ExecutionLocation, GenerationId, PersonaProfile, ResponseControls,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, SessionId, TurnId,
    UnixTimestampMillis, MAX_CONVERSATION_MESSAGE_BYTES,
};
use conversation_runtime::{ConversationQualityController, TextTurnEventStream, TextTurnRuntime};
use tempfile::tempdir;
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn completion_emits_ordered_text_lifecycle_without_speech() {
    let turn_id = TurnId::new(1);
    let language = Arc::new(MockGenerationLanguageModel::new(["hello", " world"]));
    let runtime = TextTurnRuntime::new(language).with_quality_controller(controller());

    let mut events = runtime
        .start_turn(turn_id, GenerationId::new(1), "question")
        .await
        .unwrap();
    let observed = drain(&mut events).await;

    assert_eq!(observed.len(), 6);
    assert_eq!(observed[0], RuntimeEvent::TurnStarted { turn_id });
    assert!(matches!(observed[1], RuntimeEvent::QualityResolved { .. }));
    assert_eq!(
        observed[2],
        RuntimeEvent::TextDelta {
            turn_id,
            delta: "hello".into(),
        }
    );
    assert!(matches!(
        observed[3],
        RuntimeEvent::Timing {
            turn_id: event_turn_id,
            milestone: RuntimeTimingMilestone::FirstTextDelta,
            ..
        } if event_turn_id == turn_id
    ));
    assert_eq!(
        observed[4],
        RuntimeEvent::TextDelta {
            turn_id,
            delta: " world".into(),
        }
    );
    assert_eq!(observed[5], RuntimeEvent::TurnCompleted { turn_id });
    assert_eq!(
        observed.iter().filter(|event| event.is_terminal()).count(),
        1
    );
    assert!(!observed.iter().any(|event| matches!(
        event,
        RuntimeEvent::SpeechStarted { .. }
            | RuntimeEvent::SpeechCompleted { .. }
            | RuntimeEvent::Playback { .. }
    )));
}

#[tokio::test]
async fn completion_adds_only_completed_exchange_to_next_history() {
    let language = Arc::new(MockGenerationLanguageModel::new(["answer"]));
    let runtime = TextTurnRuntime::new(language.clone()).with_quality_controller(controller());

    let mut first = runtime
        .start_turn(TurnId::new(1), GenerationId::new(1), "hello")
        .await
        .unwrap();
    assert_eq!(
        collect_terminal(&mut first).await,
        RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(1)
        }
    );

    let mut second = runtime
        .start_turn(TurnId::new(2), GenerationId::new(2), "again")
        .await
        .unwrap();
    assert_eq!(
        collect_terminal(&mut second).await,
        RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(2)
        }
    );

    let requests = language.requests();
    assert_eq!(requests[1].input().recent_messages().len(), 2);
    assert_eq!(requests[1].input().recent_messages()[0].text(), "hello");
    assert_eq!(requests[1].input().recent_messages()[1].text(), "answer");
}

#[tokio::test]
async fn memory_is_published_between_quality_and_language() {
    let temporary = tempdir().unwrap();
    let store = SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap();
    let provider = Arc::new(SqliteMemoryContextProvider::new(
        store,
        Arc::new(FixedClock(UnixTimestampMillis::new(2_000).unwrap())),
    ));
    let language = Arc::new(MockGenerationLanguageModel::new(["answer"]));
    let runtime = TextTurnRuntime::new(language.clone())
        .with_session_id(SessionId::new(9))
        .with_memory_provider(provider, ExecutionLocation::Local)
        .unwrap();

    let mut events = runtime
        .start_turn(TurnId::new(1), GenerationId::new(1), "question")
        .await
        .unwrap();
    let observed = drain_with_timeout(&mut events).await;

    let quality = event_index(&observed, |event| {
        matches!(event, RuntimeEvent::QualityResolved { .. })
    });
    let memory = event_index(&observed, |event| {
        matches!(event, RuntimeEvent::MemoryRetrieved { .. })
    });
    let text = event_index(&observed, |event| {
        matches!(event, RuntimeEvent::TextDelta { .. })
    });
    assert!(quality < memory);
    assert!(memory < text);
    assert_single_terminal(
        &observed,
        RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(1),
        },
    );
    assert_eq!(language.requests().len(), 1);
}

#[tokio::test]
async fn cancellation_waits_for_blocked_language_cleanup_and_excludes_partial_history() {
    let language = Arc::new(ReusableLanguage::new(FirstBehavior::WaitForCancellation));
    let runtime = TextTurnRuntime::new(language.clone());
    let first_turn = TurnId::new(1);
    let first_generation = GenerationId::new(1);
    let mut first = runtime
        .start_turn(first_turn, first_generation, "cancel me")
        .await
        .unwrap();

    receive_through_first_delta(&mut first).await;
    runtime
        .interrupt(first_turn, first_generation)
        .await
        .unwrap();
    let first_observed = drain_with_timeout(&mut first).await;
    assert_single_terminal(
        &first_observed,
        RuntimeEvent::TurnCancelled {
            turn_id: first_turn,
        },
    );
    assert!(language.cleanup_finished.load(Ordering::Acquire));

    let second_turn = TurnId::new(2);
    let mut second = runtime
        .start_turn(second_turn, GenerationId::new(2), "after cancellation")
        .await
        .unwrap();
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: second_turn,
        },
    );
    let requests = language.requests();
    assert!(requests[1].input().recent_messages().is_empty());
}

#[tokio::test]
async fn interruption_unblocks_a_saturated_event_consumer_before_it_drains() {
    let language = Arc::new(MockGenerationLanguageModel::new(std::iter::repeat_n(
        "x", 64,
    )));
    let runtime = TextTurnRuntime::new(language.clone());
    let first_turn = TurnId::new(1);
    let first_generation = GenerationId::new(1);
    let mut first = runtime
        .start_turn(first_turn, first_generation, "blocked consumer")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), language.wait_for_blocked_send())
        .await
        .expect("language producer never encountered bounded backpressure");
    runtime
        .interrupt(first_turn, first_generation)
        .await
        .unwrap();

    let mut second = timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .start_turn(TurnId::new(2), GenerationId::new(2), "runtime reuse")
                .await
            {
                Ok(events) => break events,
                Err(error) if error.kind() == RuntimeErrorKind::InvalidState => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected reuse error: {error}"),
            }
        }
    })
    .await
    .expect("blocked event consumer prevented cancellation cleanup");

    let first_observed = drain_with_timeout(&mut first).await;
    assert_single_terminal(
        &first_observed,
        RuntimeEvent::TurnCancelled {
            turn_id: first_turn,
        },
    );
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(2),
        },
    );
    assert!(language.requests()[1].input().recent_messages().is_empty());
}

#[tokio::test]
async fn wrong_interruption_identity_is_rejected_without_cancelling_the_turn() {
    let language = Arc::new(ReusableLanguage::new(FirstBehavior::WaitForCancellation));
    let runtime = TextTurnRuntime::new(language);
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(1);
    let mut first = runtime
        .start_turn(turn_id, generation_id, "keep running")
        .await
        .unwrap();
    receive_through_first_delta(&mut first).await;

    let wrong_turn = runtime
        .interrupt(TurnId::new(2), generation_id)
        .await
        .unwrap_err();
    assert_eq!(wrong_turn.kind(), RuntimeErrorKind::InvalidState);
    let wrong_generation = runtime
        .interrupt(turn_id, GenerationId::new(2))
        .await
        .unwrap_err();
    assert_eq!(wrong_generation.kind(), RuntimeErrorKind::InvalidState);

    runtime.interrupt(turn_id, generation_id).await.unwrap();
    let observed = drain_with_timeout(&mut first).await;
    assert_single_terminal(&observed, RuntimeEvent::TurnCancelled { turn_id });
}

#[tokio::test]
async fn completed_turn_requires_both_identifiers_to_increase_strictly() {
    for (invalid_turn, invalid_generation) in [
        (TurnId::new(10), GenerationId::new(21)),
        (TurnId::new(11), GenerationId::new(20)),
        (TurnId::new(9), GenerationId::new(19)),
    ] {
        let language = Arc::new(MockGenerationLanguageModel::new(["answer"]));
        let runtime = TextTurnRuntime::new(language);
        let mut first = runtime
            .start_turn(TurnId::new(10), GenerationId::new(20), "first")
            .await
            .unwrap();
        let first_observed = drain_with_timeout(&mut first).await;
        assert_single_terminal(
            &first_observed,
            RuntimeEvent::TurnCompleted {
                turn_id: TurnId::new(10),
            },
        );

        let error = runtime
            .start_turn(invalid_turn, invalid_generation, "invalid identity")
            .await
            .err()
            .expect("duplicate or lower identity was accepted");
        assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
        assert_eq!(error.stage(), RuntimeStage::Runtime);

        let mut next = runtime
            .start_turn(TurnId::new(11), GenerationId::new(21), "valid identity")
            .await
            .unwrap();
        let next_observed = drain_with_timeout(&mut next).await;
        assert_single_terminal(
            &next_observed,
            RuntimeEvent::TurnCompleted {
                turn_id: TurnId::new(11),
            },
        );
    }
}

#[tokio::test]
async fn adapter_error_discards_partial_history_and_allows_reuse() {
    let language = Arc::new(ReusableLanguage::new(FirstBehavior::Error));
    let runtime = TextTurnRuntime::new(language.clone());
    let first_turn = TurnId::new(1);
    let mut first = runtime
        .start_turn(first_turn, GenerationId::new(1), "first request")
        .await
        .unwrap();
    let first_observed = drain_with_timeout(&mut first).await;
    let terminal = single_terminal(&first_observed);
    assert!(matches!(
        terminal,
        RuntimeEvent::TurnFailed { turn_id, error }
            if *turn_id == first_turn && error.stage() == RuntimeStage::LanguageModel
    ));
    assert!(language.cleanup_finished.load(Ordering::Acquire));

    let second_turn = TurnId::new(2);
    let mut second = runtime
        .start_turn(second_turn, GenerationId::new(2), "second request")
        .await
        .unwrap();
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: second_turn,
        },
    );
    assert!(language.requests()[1].input().recent_messages().is_empty());
}

#[tokio::test]
async fn adapter_panic_is_contained_discards_history_and_allows_reuse() {
    let language = Arc::new(ReusableLanguage::new(FirstBehavior::Panic));
    let runtime = TextTurnRuntime::new(language.clone());
    let first_turn = TurnId::new(1);
    let mut first = runtime
        .start_turn(first_turn, GenerationId::new(1), "panic request")
        .await
        .unwrap();
    let first_observed = drain_with_timeout(&mut first).await;
    let terminal = single_terminal(&first_observed);
    assert!(matches!(
        terminal,
        RuntimeEvent::TurnFailed { turn_id, error }
            if *turn_id == first_turn
                && error.stage() == RuntimeStage::LanguageModel
                && error.message() == "generation language adapter panicked"
    ));

    let second_turn = TurnId::new(2);
    let mut second = runtime
        .start_turn(second_turn, GenerationId::new(2), "recovery request")
        .await
        .unwrap();
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: second_turn,
        },
    );
    assert!(language.requests()[1].input().recent_messages().is_empty());
}

#[tokio::test]
async fn memory_failure_prevents_language_and_allows_reuse() {
    let language = Arc::new(MockGenerationLanguageModel::new(["not reached"]));
    let runtime = TextTurnRuntime::new(language.clone())
        .with_memory_provider(Arc::new(FailingMemoryProvider), ExecutionLocation::Local)
        .unwrap();

    for value in 1..=2 {
        let turn_id = TurnId::new(value);
        let mut events = runtime
            .start_turn(turn_id, GenerationId::new(value), "memory request")
            .await
            .unwrap();
        let observed = drain_with_timeout(&mut events).await;
        let terminal = single_terminal(&observed);
        assert!(matches!(
            terminal,
            RuntimeEvent::TurnFailed { turn_id: event_turn_id, error }
                if *event_turn_id == turn_id && error.stage() == RuntimeStage::Memory
        ));
    }
    assert!(language.requests().is_empty());
}

#[tokio::test]
async fn async_memory_panic_fails_at_memory_stage_and_allows_reuse() {
    let temporary = tempdir().unwrap();
    let store = SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap();
    let provider = Arc::new(PanicOnceMemoryProvider {
        inner: SqliteMemoryContextProvider::new(
            store,
            Arc::new(FixedClock(UnixTimestampMillis::new(2_000).unwrap())),
        ),
        panicked: AtomicBool::new(false),
    });
    let language = Arc::new(MockGenerationLanguageModel::new(["recovered"]));
    let runtime = TextTurnRuntime::new(language.clone())
        .with_memory_provider(provider, ExecutionLocation::Local)
        .unwrap();

    let first_turn = TurnId::new(1);
    let mut first = runtime
        .start_turn(first_turn, GenerationId::new(1), "panic in memory")
        .await
        .unwrap();
    let first_observed = drain_with_timeout(&mut first).await;
    let terminal = single_terminal(&first_observed);
    assert!(matches!(
        terminal,
        RuntimeEvent::TurnFailed { turn_id, error }
            if *turn_id == first_turn
                && error.stage() == RuntimeStage::Memory
                && error.message() == "memory context provider panicked"
    ));

    let second_turn = TurnId::new(2);
    let mut second = runtime
        .start_turn(second_turn, GenerationId::new(2), "after memory panic")
        .await
        .unwrap();
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: second_turn,
        },
    );
    assert_eq!(language.requests().len(), 1);
    assert!(language.requests()[0].input().recent_messages().is_empty());
}

#[tokio::test]
async fn empty_language_output_fails_and_is_absent_from_next_history() {
    assert_uncommittable_output_fails(
        FirstBehavior::Empty,
        "generation language response is empty",
    )
    .await;
}

#[tokio::test]
async fn over_history_limit_output_fails_and_is_absent_from_next_history() {
    assert_uncommittable_output_fails(
        FirstBehavior::OverHistoryLimit,
        "generation language response exceeds the completed history limit",
    )
    .await;
}

#[tokio::test]
async fn dropped_event_consumer_cleans_owned_work_and_allows_reuse() {
    let language = Arc::new(ReusableLanguage::new(FirstBehavior::WaitForCancellation));
    let runtime = TextTurnRuntime::new(language.clone());
    let mut dropped = runtime
        .start_turn(TurnId::new(1), GenerationId::new(1), "drop this stream")
        .await
        .unwrap();
    receive_through_first_delta(&mut dropped).await;

    drop(dropped);
    timeout(Duration::from_secs(1), language.wait_for_cleanup())
        .await
        .expect("dropped consumer did not clean language work");
    let mut retained = timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .start_turn(TurnId::new(2), GenerationId::new(2), "reuse after drop")
                .await
            {
                Ok(events) => break events,
                Err(error) if error.kind() == RuntimeErrorKind::InvalidState => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected reuse error: {error}"),
            }
        }
    })
    .await
    .expect("dropped consumer did not release the active turn");

    let observed = drain_with_timeout(&mut retained).await;
    assert_single_terminal(
        &observed,
        RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(2),
        },
    );
    assert!(language.requests()[1].input().recent_messages().is_empty());
}

async fn drain(events: &mut TextTurnEventStream) -> Vec<RuntimeEvent> {
    let mut observed = Vec::new();
    while let Some(event) = events.recv().await {
        observed.push(event);
    }
    observed
}

async fn collect_terminal(events: &mut TextTurnEventStream) -> RuntimeEvent {
    let observed = drain(events).await;
    let terminals = observed
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    terminals.into_iter().next().unwrap()
}

async fn drain_with_timeout(events: &mut TextTurnEventStream) -> Vec<RuntimeEvent> {
    timeout(Duration::from_secs(1), drain(events))
        .await
        .expect("text turn event stream did not terminate")
}

async fn receive_through_first_delta(events: &mut TextTurnEventStream) {
    timeout(Duration::from_secs(1), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("text turn ended before publishing a delta");
            if matches!(event, RuntimeEvent::TextDelta { .. }) {
                return;
            }
        }
    })
    .await
    .expect("text turn did not publish its first delta");
}

async fn assert_uncommittable_output_fails(first_behavior: FirstBehavior, expected_message: &str) {
    let language = Arc::new(ReusableLanguage::new(first_behavior));
    let runtime = TextTurnRuntime::new(language.clone());
    let first_turn = TurnId::new(1);
    let mut first = runtime
        .start_turn(first_turn, GenerationId::new(1), "first request")
        .await
        .unwrap();
    let first_observed = drain_with_timeout(&mut first).await;
    let terminal = single_terminal(&first_observed);
    assert!(matches!(
        terminal,
        RuntimeEvent::TurnFailed { turn_id, error }
            if *turn_id == first_turn
                && error.stage() == RuntimeStage::LanguageModel
                && error.message() == expected_message
    ));

    let second_turn = TurnId::new(2);
    let mut second = runtime
        .start_turn(second_turn, GenerationId::new(2), "second request")
        .await
        .unwrap();
    let second_observed = drain_with_timeout(&mut second).await;
    assert_single_terminal(
        &second_observed,
        RuntimeEvent::TurnCompleted {
            turn_id: second_turn,
        },
    );
    assert!(language.requests()[1].input().recent_messages().is_empty());
}

fn event_index(events: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> usize {
    events
        .iter()
        .position(predicate)
        .expect("required event was not published")
}

fn single_terminal(events: &[RuntimeEvent]) -> &RuntimeEvent {
    let terminals = events
        .iter()
        .filter(|event| event.is_terminal())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    terminals[0]
}

fn assert_single_terminal(events: &[RuntimeEvent], expected: RuntimeEvent) {
    assert_eq!(single_terminal(events), &expected);
}

fn controller() -> ConversationQualityController {
    ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    )
}

struct FixedClock(UnixTimestampMillis);

impl MemoryClock for FixedClock {
    fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
        Ok(self.0)
    }
}

#[derive(Clone, Copy)]
enum FirstBehavior {
    WaitForCancellation,
    Error,
    Panic,
    Empty,
    OverHistoryLimit,
}

struct ReusableLanguage {
    first_behavior: FirstBehavior,
    requests: StdMutex<Vec<GenerationLanguageRequest>>,
    cleanup_finished: Arc<AtomicBool>,
    cleanup_notified: Arc<Notify>,
}

impl ReusableLanguage {
    fn new(first_behavior: FirstBehavior) -> Self {
        Self {
            first_behavior,
            requests: StdMutex::new(Vec::new()),
            cleanup_finished: Arc::new(AtomicBool::new(false)),
            cleanup_notified: Arc::new(Notify::new()),
        }
    }

    fn requests(&self) -> Vec<GenerationLanguageRequest> {
        self.requests
            .lock()
            .expect("reusable language requests lock poisoned")
            .clone()
    }

    async fn wait_for_cleanup(&self) {
        loop {
            let notified = self.cleanup_notified.notified();
            if self.cleanup_finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl GenerationLanguageModel for ReusableLanguage {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        self.requests
            .lock()
            .expect("reusable language requests lock poisoned")
            .push(request.clone());
        if request.generation_id() == GenerationId::new(1)
            && matches!(self.first_behavior, FirstBehavior::Panic)
        {
            panic!("scripted generation panic");
        }

        let (sender, receiver) = mpsc::channel(1);
        let first_behavior = self.first_behavior;
        let cleanup_finished = Arc::clone(&self.cleanup_finished);
        let cleanup_notified = Arc::clone(&self.cleanup_notified);
        tokio::spawn(async move {
            if request.generation_id() == GenerationId::new(1)
                && matches!(first_behavior, FirstBehavior::Empty)
            {
                return;
            }
            let delta_text = if request.generation_id() != GenerationId::new(1) {
                "recovered".to_owned()
            } else if matches!(first_behavior, FirstBehavior::OverHistoryLimit) {
                "x".repeat(MAX_CONVERSATION_MESSAGE_BYTES)
            } else {
                "partial".to_owned()
            };
            let delta =
                GenerationTextDelta::new(request.turn_id(), request.generation_id(), delta_text);
            if sender.send(Ok(delta)).await.is_err() {
                return;
            }
            if request.generation_id() != GenerationId::new(1) {
                return;
            }
            match first_behavior {
                FirstBehavior::WaitForCancellation => {
                    cancellation.cancelled().await;
                    cleanup_finished.store(true, Ordering::Release);
                    cleanup_notified.notify_waiters();
                }
                FirstBehavior::Error => {
                    let _ = sender
                        .send(Err(AdapterError::new("generation failed")))
                        .await;
                    cleanup_finished.store(true, Ordering::Release);
                    cleanup_notified.notify_waiters();
                }
                FirstBehavior::Panic => unreachable!(),
                FirstBehavior::Empty | FirstBehavior::OverHistoryLimit => {}
            }
        });
        receiver
    }
}

struct FailingMemoryProvider;

impl MemoryContextProvider for FailingMemoryProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Local
    }

    fn retrieve(
        &self,
        _turn_id: TurnId,
        _query: String,
        _cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        Box::pin(async { Err(MemoryStoreError::cancelled()) })
    }
}

struct PanicOnceMemoryProvider {
    inner: SqliteMemoryContextProvider,
    panicked: AtomicBool,
}

impl MemoryContextProvider for PanicOnceMemoryProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Local
    }

    fn retrieve(
        &self,
        turn_id: TurnId,
        query: String,
        cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        if !self.panicked.swap(true, Ordering::AcqRel) {
            return Box::pin(async {
                tokio::task::yield_now().await;
                panic!("scripted async memory panic");
            });
        }
        self.inner.retrieve(turn_id, query, cancellation)
    }
}
