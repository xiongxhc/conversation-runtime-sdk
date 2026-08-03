use std::sync::Arc;

use conversation_memory::{
    MemoryClock, MemoryContextProvider, MemoryStore, MemoryStoreErrorKind,
    SqliteMemoryContextProvider, SqliteMemoryStore,
};
use conversation_protocol::{
    ExecutionLocation, MemoryConfidence, MemoryDraft, MemoryKind, MemoryProvenance,
    MemoryProvenanceKind, MemoryRetention, TurnId, UnixTimestampMillis,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

struct FixedClock(UnixTimestampMillis);

impl MemoryClock for FixedClock {
    fn now(&self) -> conversation_memory::MemoryStoreResult<UnixTimestampMillis> {
        Ok(self.0)
    }
}

#[tokio::test]
async fn sqlite_provider_runs_bounded_retrieval_on_an_async_boundary() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let record = store
        .create(
            MemoryDraft::new(
                MemoryKind::Semantic,
                "Local project context",
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
            .unwrap(),
        )
        .unwrap();
    let provider = SqliteMemoryContextProvider::new(store, Arc::new(FixedClock(timestamp(2_000))));

    assert_eq!(provider.execution_location(), ExecutionLocation::Local);
    let retrieval = provider
        .retrieve(
            TurnId::new(4),
            "local project".to_owned(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(retrieval.items().len(), 1);
    assert_eq!(retrieval.items()[0].memory_id(), record.id());
    assert_eq!(retrieval.trace().turn_id(), TurnId::new(4));
}

#[tokio::test]
async fn sqlite_provider_waits_for_cancelled_retrieval_cleanup() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let provider = SqliteMemoryContextProvider::new(store, Arc::new(FixedClock(timestamp(2_000))));
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = provider
        .retrieve(TurnId::new(5), "private query".to_owned(), cancellation)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::Cancelled);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let traces: i64 = connection
        .query_row("SELECT count(*) FROM retrieval_traces", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(traces, 0);
}
