use std::fs;
use std::path::{Path, PathBuf};

use conversation_memory::{MemoryStore, MemoryStoreErrorKind, NeverCancelled, SqliteMemoryStore};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryId, MemoryKind, MemoryPatch, MemoryProvenance,
    MemoryProvenanceKind, MemoryRetention, MemoryRetrievalRequest, PersonId, TurnId,
    UnixTimestampMillis,
};
use rusqlite::Connection;
use tempfile::{tempdir, TempDir};

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance() -> MemoryProvenance {
    provenance_of(MemoryProvenanceKind::UserProvided)
}

fn provenance_of(kind: MemoryProvenanceKind) -> MemoryProvenance {
    MemoryProvenance::new(
        kind,
        "settings",
        timestamp(900),
        "local-user",
        None::<String>,
    )
    .unwrap()
}

fn draft(content: &str) -> MemoryDraft {
    MemoryDraft::new(
        MemoryKind::Semantic,
        content,
        provenance(),
        MemoryConfidence::new(800).unwrap(),
        timestamp(1_000),
        MemoryRetention::UntilDeleted,
    )
    .unwrap()
}

fn store() -> (TempDir, PathBuf, SqliteMemoryStore) {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    (temporary, database, store)
}

/// People are created with SQL here; the people API arrives with its own tests.
fn insert_person(database: &Path) -> PersonId {
    let connection = Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO people (is_owner, maturity, created_at_ms) VALUES (0, 'mature', 0)",
            [],
        )
        .unwrap();
    PersonId::new(connection.last_insert_rowid() as u64).unwrap()
}

fn retrieved(store: &SqliteMemoryStore, query: &str, subject: Option<PersonId>) -> Vec<MemoryId> {
    let mut request =
        MemoryRetrievalRequest::new(TurnId::new(1), query, timestamp(2_000), 8, 8_192).unwrap();
    if let Some(subject) = subject {
        request = request.with_subject(subject);
    }
    store
        .retrieve(request, &NeverCancelled)
        .unwrap()
        .items()
        .iter()
        .map(|item| item.memory_id())
        .collect()
}

#[test]
fn upgraded_memories_belong_to_the_owner() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory-v1.sqlite3"),
        &database,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let store = SqliteMemoryStore::open(&database).unwrap();
    let records = store.list(timestamp(2_000)).unwrap();
    assert_eq!(records.len(), 3);
    assert!(records
        .iter()
        .all(|record| record.subject() == PersonId::OWNER));
}

#[test]
fn created_memory_keeps_its_subject() {
    let (_temporary, database, store) = store();
    let jack = insert_person(&database);
    let record = store
        .create(draft("Jack likes tea").with_subject(jack))
        .unwrap();
    assert_eq!(record.subject(), jack);
    assert_eq!(
        store
            .inspect(record.id(), timestamp(1_100))
            .unwrap()
            .subject(),
        jack
    );
}

#[test]
fn memory_for_an_unknown_person_is_refused() {
    let (_temporary, _database, store) = store();
    let error = store
        .create(draft("tea").with_subject(PersonId::new(99).unwrap()))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::NotFound);
    assert!(store.list(timestamp(1_100)).unwrap().is_empty());
}

#[test]
fn unscoped_retrieval_returns_every_subject() {
    let (_temporary, database, store) = store();
    let jack = insert_person(&database);
    let owner = store.create(draft("tea at noon")).unwrap();
    let jacks = store
        .create(draft("tea with lemon").with_subject(jack))
        .unwrap();
    let mut found = retrieved(&store, "tea", None);
    found.sort();
    assert_eq!(found, vec![owner.id(), jacks.id()]);
}

#[test]
fn subject_scope_returns_only_that_person() {
    let (_temporary, database, store) = store();
    let jack = insert_person(&database);
    let owner = store.create(draft("tea at noon")).unwrap();
    let jacks = store
        .create(draft("tea with lemon").with_subject(jack))
        .unwrap();
    assert_eq!(retrieved(&store, "tea", Some(jack)), vec![jacks.id()]);
    assert_eq!(
        retrieved(&store, "tea", Some(PersonId::OWNER)),
        vec![owner.id()]
    );
}

#[test]
fn subject_scope_excludes_other_peoples_pinned_matches() {
    let (_temporary, database, store) = store();
    let jack = insert_person(&database);
    let owner = store.create(draft("tea at noon")).unwrap();
    store
        .set_pinned(owner.id(), owner.revision(), true, timestamp(1_100))
        .unwrap();
    assert!(retrieved(&store, "tea", Some(jack)).is_empty());
}

#[test]
fn editing_a_memory_keeps_its_subject() {
    let (_temporary, database, store) = store();
    let jack = insert_person(&database);
    let record = store
        .create(draft("Jack likes tea").with_subject(jack))
        .unwrap();
    let edited = store
        .edit(
            record.id(),
            MemoryPatch::new(
                record.revision(),
                Some("Jack likes green tea".to_owned()),
                None,
                None,
                timestamp(1_200),
                provenance_of(MemoryProvenanceKind::UserEdited),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(edited.subject(), jack);
}
