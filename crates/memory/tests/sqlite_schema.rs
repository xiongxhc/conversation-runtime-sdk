use conversation_memory::{MemoryStore, MemoryStoreErrorKind, SqliteMemoryStore};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryId, MemoryKind, MemoryPatch, MemoryProvenance,
    MemoryProvenanceKind, MemoryRetention, MemoryState, UnixTimestampMillis,
};
use rusqlite::Connection;
use tempfile::tempdir;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance(kind: MemoryProvenanceKind, source: &str, at: i64) -> MemoryProvenance {
    MemoryProvenance::new(
        kind,
        source,
        timestamp(at),
        "local-user",
        Some(format!("sha256:{source}")),
    )
    .unwrap()
}

fn draft(kind: MemoryKind, content: &str) -> MemoryDraft {
    MemoryDraft::new(
        kind,
        content,
        provenance(MemoryProvenanceKind::UserProvided, "settings", 900),
        MemoryConfidence::new(800).unwrap(),
        timestamp(1_000),
        MemoryRetention::UntilDeleted,
    )
    .unwrap()
}

#[test]
fn create_list_inspect_and_edit_round_trip_typed_records() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();

    let created = store
        .create(draft(
            MemoryKind::Semantic,
            "The runtime uses explicit local providers",
        ))
        .unwrap();
    assert_eq!(created.id(), MemoryId::new(1).unwrap());
    assert_eq!(created.state(), MemoryState::Active);
    assert_eq!(created.revision(), 1);
    assert_eq!(created.provenance().source_id(), "settings");

    let listed = store.list(timestamp(1_100)).unwrap();
    assert_eq!(listed, vec![created.clone()]);
    assert_eq!(
        store.inspect(created.id(), timestamp(1_100)).unwrap(),
        created
    );

    let edited = store
        .edit(
            created.id(),
            MemoryPatch::new(
                created.revision(),
                Some("The runtime requires explicit local providers".to_owned()),
                Some(MemoryConfidence::new(950).unwrap()),
                None,
                timestamp(1_200),
                provenance(MemoryProvenanceKind::UserEdited, "memory-probe", 1_200),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(edited.revision(), 2);
    assert_eq!(edited.updated_at(), timestamp(1_200));
    assert_eq!(
        edited.content(),
        "The runtime requires explicit local providers"
    );
    assert_eq!(edited.confidence().get(), 950);
    assert_eq!(edited.provenance().source_id(), "memory-probe");

    let stale = store
        .edit(
            created.id(),
            MemoryPatch::new(
                1,
                Some("stale overwrite".to_owned()),
                None,
                None,
                timestamp(1_300),
                provenance(MemoryProvenanceKind::UserEdited, "stale", 1_300),
            )
            .unwrap(),
        )
        .unwrap_err();
    assert_eq!(stale.kind(), MemoryStoreErrorKind::Conflict);
    assert_eq!(
        store.inspect(created.id(), timestamp(1_300)).unwrap(),
        edited
    );
}

#[test]
fn identity_and_relationship_records_start_as_candidates() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();

    let identity = store
        .create(draft(
            MemoryKind::Identity,
            "The user prefers short answers",
        ))
        .unwrap();
    let relationship = store
        .create(draft(
            MemoryKind::Relationship,
            "Shared humor may inform rapport",
        ))
        .unwrap();
    assert_eq!(identity.state(), MemoryState::Candidate);
    assert_eq!(relationship.state(), MemoryState::Candidate);
}

#[test]
fn hard_delete_removes_record_sources_and_identifier_trace_rows() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let record = store
        .create(draft(MemoryKind::Episodic, "A completed project milestone"))
        .unwrap();

    store.delete(record.id(), record.revision()).unwrap();
    let missing = store.inspect(record.id(), timestamp(2_000)).unwrap_err();
    assert_eq!(missing.kind(), MemoryStoreErrorKind::NotFound);

    let connection = Connection::open(&database).unwrap();
    let memory_count: i64 = connection
        .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    let source_count: i64 = connection
        .query_row("SELECT count(*) FROM memory_sources", [], |row| row.get(0))
        .unwrap();
    let item_count: i64 = connection
        .query_row("SELECT count(*) FROM retrieval_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!((memory_count, source_count, item_count), (0, 0, 0));
}

#[test]
fn schema_rejects_approval_rows_without_bound_confirmation_and_revision() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let record = store
        .create(draft(MemoryKind::Identity, "Approval requires evidence"))
        .unwrap();
    let connection = Connection::open(&database).unwrap();
    let result = connection.execute(
        concat!(
            "INSERT INTO memory_sources (memory_id, kind, source_id, source_timestamp_ms, actor, ",
            "content_digest, created_at_ms) VALUES (?1, 'user_approved', 'forged', 1100, ",
            "'local-user', 'sha256:forged', 1100)"
        ),
        [i64::try_from(record.id().get()).unwrap()],
    );
    assert!(result.is_err());
}
