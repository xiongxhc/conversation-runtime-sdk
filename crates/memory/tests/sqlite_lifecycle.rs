use std::fs;
use std::path::Path;

use conversation_memory::{MemoryStoreErrorKind, SqliteMemoryStore, SQLITE_APPLICATION_ID};
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn initialization_is_explicit_absolute_and_schema_identified() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("nested").join("runtime.sqlite3");

    let missing = SqliteMemoryStore::open(&database).unwrap_err();
    assert_eq!(missing.kind(), MemoryStoreErrorKind::NotInitialized);
    assert!(!database.exists());

    let relative = SqliteMemoryStore::initialize(Path::new("runtime.sqlite3")).unwrap_err();
    assert_eq!(relative.kind(), MemoryStoreErrorKind::InvalidPath);
    assert!(!Path::new("runtime.sqlite3").exists());

    let store = SqliteMemoryStore::initialize(&database).unwrap();
    assert_eq!(store.database_path(), database.as_path());
    assert!(database.is_file());
    assert!(database.parent().unwrap().is_dir());

    let connection = Connection::open(&database).unwrap();
    let application_id: u32 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .unwrap();
    let user_version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(application_id, SQLITE_APPLICATION_ID);
    assert_eq!(user_version, 1);
    drop(connection);

    SqliteMemoryStore::open(&database).unwrap();
}

#[cfg(unix)]
#[test]
fn initialization_uses_owner_only_permissions_and_rejects_symlink_leaves() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let temporary = tempdir().unwrap();
    let database = temporary.path().join("memory").join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();

    let directory_mode = fs::metadata(database.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let database_mode = fs::metadata(&database).unwrap().permissions().mode() & 0o777;
    assert_eq!(directory_mode, 0o700);
    assert_eq!(database_mode, 0o600);

    let link = temporary.path().join("linked.sqlite3");
    symlink(&database, &link).unwrap();
    let error = SqliteMemoryStore::open(&link).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::InvalidPath);
}

#[test]
fn open_rejects_foreign_newer_and_modified_schemas() {
    let temporary = tempdir().unwrap();

    let foreign = temporary.path().join("foreign.sqlite3");
    Connection::open(&foreign).unwrap();
    let error = SqliteMemoryStore::open(&foreign).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::InvalidDatabase);

    let newer = temporary.path().join("newer.sqlite3");
    SqliteMemoryStore::initialize(&newer).unwrap();
    let connection = Connection::open(&newer).unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);
    let error = SqliteMemoryStore::open(&newer).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::UnsupportedSchema);

    let modified = temporary.path().join("modified.sqlite3");
    SqliteMemoryStore::initialize(&modified).unwrap();
    let connection = Connection::open(&modified).unwrap();
    connection
        .execute(
            "UPDATE schema_migrations SET checksum = '0000000000000000' WHERE version = 1",
            [],
        )
        .unwrap();
    drop(connection);
    let error = SqliteMemoryStore::open(&modified).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::InvalidDatabase);

    let invalid_foreign_key = temporary.path().join("invalid-foreign-key.sqlite3");
    SqliteMemoryStore::initialize(&invalid_foreign_key).unwrap();
    let connection = Connection::open(&invalid_foreign_key).unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
        .execute(
            concat!(
                "INSERT INTO memory_sources (memory_id, kind, source_id, source_timestamp_ms, ",
                "actor, created_at_ms) VALUES (99, 'user_provided', 'forged', 1, 'test', 1)"
            ),
            [],
        )
        .unwrap();
    drop(connection);
    let error = SqliteMemoryStore::open(&invalid_foreign_key).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::InvalidDatabase);
}
