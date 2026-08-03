use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use conversation_memory::{
    MemoryStore, MemoryStoreErrorKind, RetrievalCancellation, SqliteMemoryStore,
    MAX_MEMORY_SCAN_RECORDS,
};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryKind, MemoryProvenance, MemoryProvenanceKind,
    MemoryRetention, MemoryRetrievalReason, MemoryRetrievalRequest, TurnId, UnixTimestampMillis,
};
use rusqlite::Connection;
use tempfile::tempdir;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn draft(
    kind: MemoryKind,
    content: &str,
    confidence: u16,
    retention: MemoryRetention,
    created_at: i64,
) -> MemoryDraft {
    MemoryDraft::new(
        kind,
        content,
        MemoryProvenance::new(
            MemoryProvenanceKind::UserProvided,
            format!("source:{created_at}"),
            timestamp(created_at),
            "local-user",
            None::<String>,
        )
        .unwrap(),
        MemoryConfidence::new(confidence).unwrap(),
        timestamp(created_at),
        retention,
    )
    .unwrap()
}

#[test]
fn retrieval_is_multilingual_stable_and_reasoned() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let dentist = store
        .create(draft(
            MemoryKind::Semantic,
            "Dentist appointment is tomorrow",
            700,
            MemoryRetention::UntilDeleted,
            1_000,
        ))
        .unwrap();
    let tea = store
        .create(draft(
            MemoryKind::Semantic,
            "用户喜欢红茶",
            900,
            MemoryRetention::UntilDeleted,
            1_100,
        ))
        .unwrap();

    let english = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(1),
                "dentist appointment",
                timestamp(2_000),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert_eq!(english.items().len(), 1);
    assert_eq!(english.items()[0].memory_id(), dentist.id());
    assert_eq!(
        english.items()[0].reason(),
        MemoryRetrievalReason::ExactPhrase
    );

    let chinese = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(2),
                "我今天想喝红茶",
                timestamp(2_100),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert_eq!(chinese.items()[0].memory_id(), tea.id());
    assert_eq!(
        chinese.items()[0].reason(),
        MemoryRetrievalReason::SharedTerm
    );
    assert_eq!(chinese.trace().selected_items(), 1);
    assert_eq!(chinese.trace().used_bytes(), "用户喜欢红茶".len());
}

#[test]
fn pinned_matches_rank_first_without_bypassing_relevance_or_budgets() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let first = store
        .create(draft(
            MemoryKind::Semantic,
            "Project Atlas uses local models",
            900,
            MemoryRetention::UntilDeleted,
            1_000,
        ))
        .unwrap();
    let second = store
        .create(draft(
            MemoryKind::Episodic,
            "Local project planning session",
            700,
            MemoryRetention::UntilDeleted,
            1_100,
        ))
        .unwrap();
    let pinned = store
        .set_pinned(second.id(), second.revision(), true, timestamp(1_200))
        .unwrap();

    let retrieval = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(3),
                "local project",
                timestamp(2_000),
                1,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert_eq!(retrieval.items()[0].memory_id(), pinned.id());
    assert_eq!(
        retrieval.items()[0].reason(),
        MemoryRetrievalReason::PinnedMatch
    );
    assert_eq!(retrieval.trace().exclusions().by_item_limit(), 1);
    assert_ne!(retrieval.items()[0].memory_id(), first.id());

    let unrelated = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(4),
                "weather forecast",
                timestamp(2_100),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert!(unrelated.items().is_empty());
    assert_eq!(unrelated.trace().exclusions().by_relevance(), 2);
}

#[test]
fn retrieval_skips_whole_records_at_item_and_byte_limits() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    store
        .create(draft(
            MemoryKind::Semantic,
            "alpha ".repeat(30).trim(),
            900,
            MemoryRetention::UntilDeleted,
            1_000,
        ))
        .unwrap();
    store
        .create(draft(
            MemoryKind::Semantic,
            "alpha short",
            800,
            MemoryRetention::UntilDeleted,
            1_100,
        ))
        .unwrap();

    let retrieval = store
        .retrieve(
            MemoryRetrievalRequest::new(TurnId::new(5), "alpha", timestamp(2_000), 2, 20).unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert_eq!(retrieval.items().len(), 1);
    assert_eq!(retrieval.items()[0].content(), "alpha short");
    assert_eq!(retrieval.trace().exclusions().by_byte_limit(), 1);
}

#[test]
fn retrieval_excludes_records_created_after_the_request_clock() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    store
        .create(draft(
            MemoryKind::Semantic,
            "future local project",
            900,
            MemoryRetention::UntilDeleted,
            2_000,
        ))
        .unwrap();

    let retrieval = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(7),
                "local project",
                timestamp(1_999),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert!(retrieval.items().is_empty());
    assert_eq!(retrieval.trace().exclusions().by_state(), 1);
}

#[test]
fn retrieval_excludes_future_updates_and_never_moves_last_use_backward() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let original = store
        .create(draft(
            MemoryKind::Semantic,
            "original project context",
            900,
            MemoryRetention::UntilDeleted,
            1_000,
        ))
        .unwrap();
    let edited = store
        .edit(
            original.id(),
            conversation_protocol::MemoryPatch::new(
                original.revision(),
                Some("updated project context".to_owned()),
                None,
                None,
                timestamp(2_000),
                MemoryProvenance::new(
                    MemoryProvenanceKind::UserEdited,
                    "edit:1",
                    timestamp(2_000),
                    "local-user",
                    None,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();

    let historical = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(8),
                "updated project",
                timestamp(1_500),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert!(historical.items().is_empty());
    assert_eq!(historical.trace().exclusions().by_state(), 1);

    store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(9),
                "updated project",
                timestamp(3_000),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    let replay = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(10),
                "updated project",
                timestamp(2_500),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    assert!(replay.items().is_empty());
    assert_eq!(replay.trace().exclusions().by_state(), 1);
    assert_eq!(
        store
            .inspect(edited.id(), timestamp(3_000))
            .unwrap()
            .last_used_at(),
        Some(timestamp(3_000))
    );
}

#[test]
fn retrieval_fails_closed_when_the_deterministic_scan_cap_is_exceeded() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let mut connection = Connection::open(&database).unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..=MAX_MEMORY_SCAN_RECORDS {
        transaction
            .execute(
                concat!(
                    "INSERT INTO memories (kind, state, content, confidence, created_at_ms, ",
                    "updated_at_ms, retention_kind) VALUES ('semantic', 'active', ?1, 500, 1000, 1000, 'until_deleted')"
                ),
                [format!("bounded scan record {index}")],
            )
            .unwrap();
        let memory_id = transaction.last_insert_rowid();
        transaction
            .execute(
                concat!(
                    "INSERT INTO memory_sources (memory_id, kind, source_id, source_timestamp_ms, actor, created_at_ms) ",
                    "VALUES (?1, 'user_provided', ?2, 1000, 'local-user', 1000)"
                ),
                rusqlite::params![memory_id, format!("source:{index}")],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let error = store
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(11),
                "bounded scan",
                timestamp(2_000),
                4,
                4_096,
            )
            .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::LimitExceeded);
}

struct CancelAfter {
    checks: AtomicUsize,
    maximum_clear_checks: usize,
}

impl RetrievalCancellation for CancelAfter {
    fn is_cancelled(&self) -> bool {
        self.checks.fetch_add(1, Ordering::SeqCst) >= self.maximum_clear_checks
    }
}

#[test]
fn cancellation_persists_no_trace_last_use_or_query_text() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let record = store
        .create(draft(
            MemoryKind::Semantic,
            "private cancellation marker",
            900,
            MemoryRetention::UntilDeleted,
            1_000,
        ))
        .unwrap();
    let secret_query = "query-must-never-reach-disk";
    let error = store
        .retrieve(
            MemoryRetrievalRequest::new(TurnId::new(6), secret_query, timestamp(2_000), 4, 4_096)
                .unwrap(),
            &CancelAfter {
                checks: AtomicUsize::new(0),
                maximum_clear_checks: 1,
            },
        )
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::Cancelled);

    let connection = Connection::open(&database).unwrap();
    let trace_count: i64 = connection
        .query_row("SELECT count(*) FROM retrieval_traces", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(trace_count, 0);
    drop(connection);
    assert_eq!(
        store
            .inspect(record.id(), timestamp(2_001))
            .unwrap()
            .last_used_at(),
        None
    );

    let bytes = fs::read(&database).unwrap();
    assert!(!bytes
        .windows(secret_query.len())
        .any(|window| window == secret_query.as_bytes()));
}
