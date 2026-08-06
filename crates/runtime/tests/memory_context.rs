use std::fs;
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{
    MemoryClock, MemoryContextProvider, MemoryProviderFuture, MemoryStore, MemoryStoreError,
    MemoryStoreResult, SqliteMemoryContextProvider, SqliteMemoryStore,
};
use conversation_model_adapters::{
    MockContinuousAudioOutput, MockGenerationLanguageModel, MockStreamingSpeechSynthesizer,
};
use conversation_protocol::{
    ConversationMode, ExecutionLocation, MemoryConfidence, MemoryDraft, MemoryKind,
    MemoryProvenance, MemoryProvenanceKind, MemoryRetention, PersonaProfile, ResponseControls,
    RuntimeEvent, RuntimeStage, TurnId, UnixTimestampMillis,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, ConversationTurnSource,
    StreamingTurnEventStream, StreamingTurnRuntime,
};
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

struct FixedClock(UnixTimestampMillis);

impl MemoryClock for FixedClock {
    fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
        Ok(self.0)
    }
}

fn draft(content: &str) -> MemoryDraft {
    MemoryDraft::new(
        MemoryKind::Semantic,
        content,
        MemoryProvenance::new(
            MemoryProvenanceKind::UserProvided,
            "settings",
            timestamp(1_000),
            "local-user",
            None,
        )
        .unwrap(),
        MemoryConfidence::new(900).unwrap(),
        timestamp(1_000),
        MemoryRetention::UntilDeleted,
    )
    .unwrap()
}

fn context() -> ConversationContext {
    ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    ))
}

fn runtime(
    context: ConversationContext,
    language: Arc<MockGenerationLanguageModel>,
) -> StreamingTurnRuntime {
    StreamingTurnRuntime::new(
        context,
        language,
        Arc::new(MockStreamingSpeechSynthesizer::new([])),
        Arc::new(MockContinuousAudioOutput::new()),
    )
}

#[tokio::test]
async fn local_memory_is_retrieved_traced_and_deleted_before_later_turns() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let record = store.create(draft("Local project context")).unwrap();
    let provider = Arc::new(SqliteMemoryContextProvider::new(
        store.clone(),
        Arc::new(FixedClock(timestamp(2_000))),
    ));
    let language = Arc::new(MockGenerationLanguageModel::new(["#"]));
    let runtime = runtime(
        context()
            .with_memory_provider(provider, ExecutionLocation::Local)
            .unwrap(),
        language.clone(),
    );

    let mut first = runtime
        .start_turn(ConversationTurnSource::Text, "local project")
        .await
        .unwrap();
    let first_events = drain(&mut first).await;
    let memory_index = first_events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::MemoryRetrieved { .. }))
        .unwrap();
    let text_index = first_events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::TextDelta { .. }))
        .unwrap();
    assert!(memory_index < text_index);
    let RuntimeEvent::MemoryRetrieved { trace } = &first_events[memory_index] else {
        unreachable!();
    };
    assert_eq!(trace.selected_items(), 1);
    assert_eq!(language.requests()[0].input().memory_items().len(), 1);
    assert_eq!(
        language.requests()[0].input().memory_items()[0].memory_id(),
        record.id()
    );

    store.delete(record.id(), record.revision()).unwrap();
    let mut second = runtime
        .start_turn(ConversationTurnSource::Text, "local project")
        .await
        .unwrap();
    let second_events = drain(&mut second).await;
    let RuntimeEvent::MemoryRetrieved { trace } = second_events
        .iter()
        .find(|event| matches!(event, RuntimeEvent::MemoryRetrieved { .. }))
        .unwrap()
    else {
        unreachable!();
    };
    assert_eq!(trace.selected_items(), 0);
    assert!(language.requests()[1].input().memory_items().is_empty());
}

#[tokio::test]
async fn configured_memory_failure_fails_closed_before_language_generation() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let provider = Arc::new(SqliteMemoryContextProvider::new(
        store,
        Arc::new(FixedClock(timestamp(2_000))),
    ));
    fs::remove_file(&database).unwrap();
    let language = Arc::new(MockGenerationLanguageModel::new(["not reached"]));
    let runtime = runtime(
        context()
            .with_memory_provider(provider, ExecutionLocation::Local)
            .unwrap(),
        language.clone(),
    );

    let mut events = runtime
        .start_turn(ConversationTurnSource::Text, "question")
        .await
        .unwrap();
    let observed = drain(&mut events).await;
    let terminal = observed.iter().find(|event| event.is_terminal()).unwrap();
    let RuntimeEvent::TurnFailed { error, .. } = terminal else {
        panic!("memory failure did not fail the turn");
    };
    assert_eq!(error.stage(), RuntimeStage::Memory);
    assert!(language.requests().is_empty());
}

struct BlockingProvider {
    started: Arc<Notify>,
}

impl MemoryContextProvider for BlockingProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Local
    }

    fn retrieve(
        &self,
        _turn_id: TurnId,
        _query: String,
        cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            cancellation.cancelled().await;
            Err(MemoryStoreError::cancelled())
        })
    }
}

#[tokio::test]
async fn interruption_cancels_and_awaits_memory_before_skipping_generation() {
    let started = Arc::new(Notify::new());
    let language = Arc::new(MockGenerationLanguageModel::new(["not reached"]));
    let runtime = runtime(
        context()
            .with_memory_provider(
                Arc::new(BlockingProvider {
                    started: Arc::clone(&started),
                }),
                ExecutionLocation::Local,
            )
            .unwrap(),
        language.clone(),
    );
    let mut events = runtime
        .start_turn(ConversationTurnSource::Text, "question")
        .await
        .unwrap();
    let identity = events.identity();
    timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    runtime
        .interrupt(identity.turn_id(), identity.generation_id())
        .await
        .unwrap();

    let observed = drain(&mut events).await;
    assert!(observed
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnCancelled { .. })));
    assert!(language.requests().is_empty());
}

struct RemoteProvider;

impl MemoryContextProvider for RemoteProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Remote
    }

    fn retrieve(
        &self,
        _turn_id: TurnId,
        _query: String,
        _cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        unreachable!()
    }
}

#[test]
fn memory_attachment_rejects_remote_memory_or_language_execution() {
    let remote_store = context()
        .with_memory_provider(Arc::new(RemoteProvider), ExecutionLocation::Local)
        .err()
        .expect("remote memory provider should be rejected");
    assert_eq!(remote_store.stage(), RuntimeStage::Memory);

    let temporary = tempdir().unwrap();
    let store = SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap();
    let local_provider = Arc::new(SqliteMemoryContextProvider::new(
        store,
        Arc::new(FixedClock(timestamp(2_000))),
    ));
    let remote_language = context()
        .with_memory_provider(local_provider, ExecutionLocation::Remote)
        .err()
        .expect("remote language execution should be rejected");
    assert_eq!(remote_language.stage(), RuntimeStage::Memory);
}

async fn drain(stream: &mut StreamingTurnEventStream) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}
