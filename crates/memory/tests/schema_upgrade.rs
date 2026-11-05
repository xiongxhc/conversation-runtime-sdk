use std::fs;
use std::path::{Path, PathBuf};

use conversation_memory::{MemoryStore, MemoryStoreErrorKind, SqliteMemoryStore};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryKind, MemoryProvenance, MemoryProvenanceKind,
    MemoryRetention, UnixTimestampMillis,
};
use rusqlite::types::Value;
use rusqlite::Connection;
use tempfile::{tempdir, TempDir};

const MEMORY_COLUMNS: &str = concat!(
    "id, kind, state, content, confidence, created_at_ms, updated_at_ms, retention_kind, ",
    "expires_at_ms, session_id, pinned, prior_retention_kind, prior_expires_at_ms, ",
    "prior_session_id, revision, last_used_at_ms, last_retrieval_reason"
);

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn draft(content: &str) -> MemoryDraft {
    MemoryDraft::new(
        MemoryKind::Semantic,
        content,
        MemoryProvenance::new(
            MemoryProvenanceKind::UserProvided,
            "settings",
            timestamp(900),
            "local-user",
            None::<String>,
        )
        .unwrap(),
        MemoryConfidence::new(800).unwrap(),
        timestamp(1_000),
        MemoryRetention::UntilDeleted,
    )
    .unwrap()
}

fn copy_fixture() -> (TempDir, PathBuf) {
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
    (temporary, database)
}

fn raw(database: &Path) -> Connection {
    Connection::open(database).unwrap()
}

fn rows(database: &Path, sql: &str) -> Vec<Vec<Value>> {
    let connection = raw(database);
    let mut statement = connection.prepare(sql).unwrap();
    let width = statement.column_count();
    statement
        .query_map([], |row| {
            (0..width)
                .map(|index| row.get::<_, Value>(index))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn record_rows(database: &Path) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    (
        rows(
            database,
            &format!("SELECT {MEMORY_COLUMNS} FROM memories ORDER BY id"),
        ),
        rows(database, "SELECT * FROM memory_sources ORDER BY id"),
    )
}

fn pragma_user_version(database: &Path) -> u32 {
    raw(database)
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn table_exists(database: &Path, table: &str) -> bool {
    raw(database)
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

fn schema(database: &Path) -> Vec<Vec<Value>> {
    rows(
        database,
        "SELECT type, name, sql FROM sqlite_schema ORDER BY type, name",
    )
}

#[test]
fn upgrade_preserves_every_record_and_keeps_ids_increasing() {
    let (_temporary, database) = copy_fixture();
    let before = record_rows(&database);
    assert_eq!(before.0.len(), 3);

    let store = SqliteMemoryStore::open(&database).unwrap();

    assert_eq!(pragma_user_version(&database), 2);
    assert_eq!(record_rows(&database), before);
    assert_eq!(
        rows(&database, "SELECT DISTINCT subject_person_id FROM memories"),
        vec![vec![Value::Integer(1)]]
    );
    let created = store.create(draft("new after upgrade")).unwrap();
    assert_eq!(created.id().get(), 5);
}

#[test]
fn upgraded_database_opens_again_without_change() {
    let (_temporary, database) = copy_fixture();
    SqliteMemoryStore::open(&database).unwrap();
    let after_first = (schema(&database), record_rows(&database));
    SqliteMemoryStore::open(&database).unwrap();
    assert_eq!((schema(&database), record_rows(&database)), after_first);
}

#[test]
fn fresh_database_and_upgraded_database_have_the_same_schema() {
    let (_upgraded_directory, upgraded) = copy_fixture();
    SqliteMemoryStore::open(&upgraded).unwrap();
    let fresh_directory = tempdir().unwrap();
    let fresh = fresh_directory.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&fresh).unwrap();

    assert_eq!(schema(&fresh), schema(&upgraded));
}

#[test]
fn owner_person_exists_after_upgrade_and_after_initialize() {
    let owner = vec![vec![Value::Integer(1), Value::Integer(1)]];
    let (_temporary, upgraded) = copy_fixture();
    SqliteMemoryStore::open(&upgraded).unwrap();
    assert_eq!(rows(&upgraded, "SELECT id, is_owner FROM people"), owner);

    let fresh_directory = tempdir().unwrap();
    let fresh = fresh_directory.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&fresh).unwrap();
    assert_eq!(rows(&fresh, "SELECT id, is_owner FROM people"), owner);
}

#[test]
fn invalid_version_1_database_is_refused_and_left_unchanged() {
    let (_temporary, database) = copy_fixture();
    raw(&database)
        .execute(
            "UPDATE schema_migrations SET checksum = '0000000000000000'",
            [],
        )
        .unwrap();
    let before = record_rows(&database);

    let error = SqliteMemoryStore::open(&database).unwrap_err();

    assert_eq!(error.kind(), MemoryStoreErrorKind::InvalidDatabase);
    assert_eq!(pragma_user_version(&database), 1);
    assert!(!table_exists(&database, "people"));
    assert_eq!(record_rows(&database), before);
}

#[test]
fn unknown_future_version_is_refused() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    raw(&database)
        .pragma_update(None, "user_version", 3)
        .unwrap();

    let error = SqliteMemoryStore::open(&database).unwrap_err();

    assert_eq!(error.kind(), MemoryStoreErrorKind::UnsupportedSchema);
}
