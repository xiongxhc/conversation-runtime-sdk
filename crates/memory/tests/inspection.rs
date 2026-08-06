use conversation_memory::{MemoryStore, MemoryStoreErrorKind, SqliteMemoryStore};
use conversation_protocol::{
    MemoryApproval, MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance,
    MemoryProvenanceKind, MemoryRetention, MemoryRetrievalRequest, MemoryState, TurnId,
    UnixTimestampMillis,
};
use rusqlite::Connection;
use tempfile::tempdir;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance(
    kind: MemoryProvenanceKind,
    source_id: impl Into<String>,
    at: i64,
) -> MemoryProvenance {
    MemoryProvenance::new(kind, source_id, timestamp(at), "local-user", None).unwrap()
}

fn draft(
    kind: MemoryKind,
    content: impl Into<String>,
    created_at: i64,
    retention: MemoryRetention,
) -> MemoryDraft {
    MemoryDraft::new(
        kind,
        content,
        provenance(
            MemoryProvenanceKind::UserProvided,
            format!("create:{created_at}"),
            created_at,
        ),
        MemoryConfidence::new(800).unwrap(),
        timestamp(created_at),
        retention,
    )
    .unwrap()
}

fn initialize_store() -> (tempfile::TempDir, SqliteMemoryStore) {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    (temporary, SqliteMemoryStore::initialize(database).unwrap())
}

fn create_semantic(
    store: &SqliteMemoryStore,
    created_at: i64,
) -> conversation_protocol::MemoryRecord {
    store
        .create(draft(
            MemoryKind::Semantic,
            format!("memory {created_at}"),
            created_at,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap()
}

fn trace_count(database: &std::path::Path) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row("SELECT count(*) FROM retrieval_traces", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn add_source_history(
    store: &SqliteMemoryStore,
    mut record: conversation_protocol::MemoryRecord,
    count: usize,
) -> conversation_protocol::MemoryRecord {
    for number in 1..count {
        let at = 2_000 + i64::try_from(number).unwrap();
        record = store
            .edit(
                record.id(),
                MemoryPatch::new(
                    record.revision(),
                    Some(format!("identity revision {number}")),
                    None,
                    None,
                    timestamp(at),
                    provenance(
                        MemoryProvenanceKind::UserEdited,
                        format!("edit:{number}"),
                        at,
                    ),
                )
                .unwrap(),
            )
            .unwrap();
    }
    record
}

fn add_approval_history(
    store: &SqliteMemoryStore,
    mut record: conversation_protocol::MemoryRecord,
    count: usize,
) -> conversation_protocol::MemoryRecord {
    for number in 1..=count {
        let approval_at = 3_000 + i64::try_from(number).unwrap() * 2;
        record = store
            .approve(
                record.id(),
                MemoryApproval::new(
                    format!("approval:{number}"),
                    "local-user",
                    timestamp(approval_at),
                    record.revision(),
                )
                .unwrap(),
            )
            .unwrap();
        if number < count {
            let edit_at = approval_at + 1;
            record = store
                .edit(
                    record.id(),
                    MemoryPatch::new(
                        record.revision(),
                        Some(format!("identity approval revision {number}")),
                        None,
                        None,
                        timestamp(edit_at),
                        provenance(
                            MemoryProvenanceKind::UserEdited,
                            format!("approval-edit:{number}"),
                            edit_at,
                        ),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
    }
    record
}

#[test]
fn list_page_returns_descending_keyset_boundaries() {
    let (_temporary, store) = initialize_store();
    for number in 1..=52 {
        create_semantic(&store, number);
    }

    let first = store.list_page(timestamp(10_000), None, 50).unwrap();
    assert_eq!(first.records().len(), 50);
    assert_eq!(first.records()[0].id().get(), 52);
    assert_eq!(first.records()[49].id().get(), 3);
    assert_eq!(first.next_before_id().unwrap().get(), 3);

    let second = store
        .list_page(timestamp(10_000), first.next_before_id(), 50)
        .unwrap();
    assert_eq!(
        second
            .records()
            .iter()
            .map(|record| record.id().get())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert!(second.next_before_id().is_none());
}

#[test]
fn list_page_handles_exact_and_empty_pages_and_rejects_invalid_limits() {
    let (_temporary, store) = initialize_store();
    for number in 1..=50 {
        create_semantic(&store, number);
    }

    let exact = store.list_page(timestamp(10_000), None, 50).unwrap();
    assert_eq!(exact.records().len(), 50);
    assert!(exact.next_before_id().is_none());

    let (_empty_temporary, empty_store) = initialize_store();
    let empty = empty_store.list_page(timestamp(10_000), None, 50).unwrap();
    assert!(empty.records().is_empty());
    assert!(empty.next_before_id().is_none());
    assert_eq!(
        store
            .list_page(timestamp(10_000), None, 0)
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::LimitExceeded
    );
    assert_eq!(
        store
            .list_page(timestamp(10_000), None, 51)
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::LimitExceeded
    );
}

#[test]
fn list_page_expires_due_records_without_changing_identifier_order() {
    let (_temporary, store) = initialize_store();
    create_semantic(&store, 1_000);
    let working = store
        .create(draft(
            MemoryKind::Working,
            "expires on this page",
            1_100,
            MemoryRetention::working(timestamp(1_200)),
        ))
        .unwrap();

    let page = store.list_page(timestamp(1_200), None, 50).unwrap();
    assert_eq!(
        page.records()
            .iter()
            .map(|record| record.id().get())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(page.records()[0].state(), MemoryState::Expired);
    assert_eq!(
        store
            .inspect(working.id(), timestamp(1_200))
            .unwrap()
            .state(),
        MemoryState::Expired
    );
}

#[test]
fn list_page_cursor_ignores_edits_and_newer_records_until_refresh() {
    let (_temporary, store) = initialize_store();
    let mut records = Vec::new();
    for number in 1..=52 {
        records.push(create_semantic(&store, number));
    }

    let first = store.list_page(timestamp(10_000), None, 50).unwrap();
    let unseen = &records[1];
    store
        .edit(
            unseen.id(),
            MemoryPatch::new(
                unseen.revision(),
                Some("edited unseen record".to_owned()),
                None,
                None,
                timestamp(10_001),
                provenance(MemoryProvenanceKind::UserEdited, "edit:unseen", 10_001),
            )
            .unwrap(),
        )
        .unwrap();
    create_semantic(&store, 10_002);

    let second = store
        .list_page(timestamp(10_002), first.next_before_id(), 50)
        .unwrap();
    assert_eq!(
        second
            .records()
            .iter()
            .map(|record| record.id().get())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(second.records()[0].content(), "edited unseen record");
    assert!(second.next_before_id().is_none());
    assert_eq!(
        store
            .list_page(timestamp(10_002), None, 50)
            .unwrap()
            .records()[0]
            .id()
            .get(),
        53
    );
}

#[test]
fn list_page_skips_deleted_unseen_records_without_duplicates() {
    let (_temporary, store) = initialize_store();
    let mut records = Vec::new();
    for number in 1..=52 {
        records.push(create_semantic(&store, number));
    }
    let first = store.list_page(timestamp(10_000), None, 50).unwrap();
    store
        .delete(records[1].id(), records[1].revision())
        .unwrap();

    let second = store
        .list_page(timestamp(10_000), first.next_before_id(), 50)
        .unwrap();
    assert_eq!(
        second
            .records()
            .iter()
            .map(|record| record.id().get())
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert!(second.next_before_id().is_none());
}

#[test]
fn list_page_does_not_write_retrieval_state() {
    let (temporary, store) = initialize_store();
    let record = store
        .create(draft(
            MemoryKind::Semantic,
            "local memory retrieval evidence",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    store
        .retrieve(
            MemoryRetrievalRequest::new(TurnId::new(1), "local memory", timestamp(2_000), 1, 4_096)
                .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    let before = store.inspect(record.id(), timestamp(2_001)).unwrap();
    let traces_before = trace_count(temporary.path().join("runtime.sqlite3").as_path());

    store.list_page(timestamp(2_002), None, 50).unwrap();

    assert_eq!(
        trace_count(temporary.path().join("runtime.sqlite3").as_path()),
        traces_before
    );
    let after = store.inspect(record.id(), timestamp(2_002)).unwrap();
    assert_eq!(after.last_used_at(), before.last_used_at());
    assert_eq!(
        after.last_retrieval_reason(),
        before.last_retrieval_reason()
    );
}

#[test]
fn inspect_bounded_returns_latest_source_and_approval_windows_oldest_first() {
    let (_temporary, store) = initialize_store();
    let source_record = store
        .create(draft(
            MemoryKind::Semantic,
            "The user prefers concise responses",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    let source_record = add_source_history(&store, source_record, 33);
    let source_inspection = store
        .inspect_bounded(source_record.id(), timestamp(4_000), 32)
        .unwrap();
    assert_eq!(source_inspection.inspection().sources().len(), 32);
    assert_eq!(
        source_inspection.inspection().sources()[0].source_id(),
        "edit:1"
    );
    assert_eq!(
        source_inspection.inspection().sources()[31].source_id(),
        source_inspection
            .inspection()
            .record()
            .provenance()
            .source_id()
    );
    assert!(source_inspection.sources_truncated());

    let approval_record = store
        .create(draft(
            MemoryKind::Identity,
            "The user prefers direct answers",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    let approval_record = add_approval_history(&store, approval_record, 33);
    let approval_inspection = store
        .inspect_bounded(approval_record.id(), timestamp(4_000), 32)
        .unwrap();
    assert_eq!(approval_inspection.inspection().approvals().len(), 32);
    assert_eq!(
        approval_inspection.inspection().approvals()[0].confirmation_id(),
        "approval:2"
    );
    assert_eq!(
        approval_inspection.inspection().approvals()[31].confirmation_id(),
        approval_inspection
            .inspection()
            .record()
            .approval()
            .unwrap()
            .confirmation_id()
    );
    assert!(approval_inspection.approvals_truncated());
}

#[test]
fn inspect_bounded_handles_exact_empty_and_invalid_history_limits() {
    let (_temporary, store) = initialize_store();
    let source_record = store
        .create(draft(
            MemoryKind::Semantic,
            "The user prefers direct answers",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    let source_record = add_source_history(&store, source_record, 32);
    let source_inspection = store
        .inspect_bounded(source_record.id(), timestamp(4_000), 32)
        .unwrap();
    assert_eq!(source_inspection.inspection().sources().len(), 32);
    assert!(!source_inspection.sources_truncated());

    let approval_record = store
        .create(draft(
            MemoryKind::Identity,
            "The user prefers concise answers",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    let approval_record = add_approval_history(&store, approval_record, 32);
    let approval_inspection = store
        .inspect_bounded(approval_record.id(), timestamp(4_000), 32)
        .unwrap();
    assert_eq!(approval_inspection.inspection().approvals().len(), 32);
    assert!(!approval_inspection.approvals_truncated());

    let semantic = create_semantic(&store, 5_000);
    assert!(store
        .inspect_bounded(semantic.id(), timestamp(5_001), 32)
        .unwrap()
        .inspection()
        .approvals()
        .is_empty());
    assert_eq!(
        store
            .inspect_bounded(approval_record.id(), timestamp(4_000), 0)
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::LimitExceeded
    );
    assert_eq!(
        store
            .inspect_bounded(approval_record.id(), timestamp(4_000), 33)
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::LimitExceeded
    );
}

#[test]
fn inspect_bounded_does_not_write_retrieval_state() {
    let (temporary, store) = initialize_store();
    let record = store
        .create(draft(
            MemoryKind::Semantic,
            "local memory inspection evidence",
            1_000,
            MemoryRetention::UntilDeleted,
        ))
        .unwrap();
    store
        .retrieve(
            MemoryRetrievalRequest::new(TurnId::new(1), "local memory", timestamp(2_000), 1, 4_096)
                .unwrap(),
            &conversation_memory::NeverCancelled,
        )
        .unwrap();
    let before = store.inspect(record.id(), timestamp(2_001)).unwrap();
    let traces_before = trace_count(temporary.path().join("runtime.sqlite3").as_path());

    let inspection = store
        .inspect_bounded(record.id(), timestamp(2_002), 32)
        .unwrap();

    assert_eq!(
        inspection.inspection().record().last_used_at(),
        before.last_used_at()
    );
    assert_eq!(
        inspection.inspection().record().last_retrieval_reason(),
        before.last_retrieval_reason()
    );
    assert_eq!(
        trace_count(temporary.path().join("runtime.sqlite3").as_path()),
        traces_before
    );
}
