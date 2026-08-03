use conversation_memory::{MemoryStore, MemoryStoreErrorKind, SqliteMemoryStore};
use conversation_protocol::{
    MemoryApproval, MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance,
    MemoryProvenanceKind, MemoryRetention, MemoryState, SessionId, UnixTimestampMillis,
};
use tempfile::tempdir;

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance(kind: MemoryProvenanceKind, source: &str, at: i64) -> MemoryProvenance {
    MemoryProvenance::new(kind, source, timestamp(at), "local-user", None::<String>).unwrap()
}

fn draft(kind: MemoryKind, retention: MemoryRetention) -> MemoryDraft {
    MemoryDraft::new(
        kind,
        format!("{kind:?} memory"),
        provenance(MemoryProvenanceKind::CompletedExchange, "turn:7", 900),
        MemoryConfidence::new(800).unwrap(),
        timestamp(1_000),
        retention,
    )
    .unwrap()
}

#[test]
fn identity_and_relationship_require_confirmed_revision_checked_approval() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let identity = store
        .create(draft(MemoryKind::Identity, MemoryRetention::UntilDeleted))
        .unwrap();
    assert_eq!(identity.state(), MemoryState::Candidate);

    let stale = store
        .approve(
            identity.id(),
            MemoryApproval::new("confirm:1", "local-user", timestamp(1_100), 2).unwrap(),
        )
        .unwrap_err();
    assert_eq!(stale.kind(), MemoryStoreErrorKind::Conflict);

    let approved = store
        .approve(
            identity.id(),
            MemoryApproval::new(
                "confirm:1",
                "local-user",
                timestamp(1_100),
                identity.revision(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(approved.state(), MemoryState::Active);
    assert_eq!(approved.revision(), 2);
    assert_eq!(
        approved.provenance().kind(),
        MemoryProvenanceKind::CompletedExchange
    );
    let approval = approved.approval().unwrap();
    assert_eq!(approval.confirmation_id(), "confirm:1");
    assert_eq!(approval.approved_revision(), identity.revision());
    assert!(approval.content_digest().starts_with("sha256:"));

    let already_active = store
        .approve(
            identity.id(),
            MemoryApproval::new("confirm:2", "local-user", timestamp(1_125), 2).unwrap(),
        )
        .unwrap_err();
    assert_eq!(already_active.kind(), MemoryStoreErrorKind::Conflict);

    let edited = store
        .edit(
            approved.id(),
            MemoryPatch::new(
                approved.revision(),
                Some("Updated identity memory".to_owned()),
                None,
                None,
                timestamp(1_150),
                provenance(MemoryProvenanceKind::UserEdited, "memory-control", 1_150),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(edited.state(), MemoryState::Candidate);
    assert_eq!(edited.provenance().kind(), MemoryProvenanceKind::UserEdited);
    assert_eq!(edited.approval(), None);

    let inspection = store
        .inspect_with_sources(edited.id(), timestamp(1_150))
        .unwrap();
    assert_eq!(inspection.sources().len(), 2);
    assert_eq!(inspection.sources()[0].source_id(), "turn:7");
    assert_eq!(inspection.sources()[1].source_id(), "memory-control");
    assert_eq!(inspection.approvals().len(), 1);
    assert_eq!(inspection.approvals()[0].confirmation_id(), "confirm:1");

    let reapproved = store
        .approve(
            edited.id(),
            MemoryApproval::new(
                "confirm:2",
                "local-user",
                timestamp(1_200),
                edited.revision(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(reapproved.state(), MemoryState::Active);
    assert_eq!(
        reapproved.approval().unwrap().confirmation_id(),
        "confirm:2"
    );

    let semantic = store
        .create(draft(MemoryKind::Semantic, MemoryRetention::UntilDeleted))
        .unwrap();
    let invalid_kind = store
        .approve(
            semantic.id(),
            MemoryApproval::new("confirm:3", "local-user", timestamp(1_300), 1).unwrap(),
        )
        .unwrap_err();
    assert_eq!(invalid_kind.kind(), MemoryStoreErrorKind::Conflict);
}

#[test]
fn working_memory_cannot_be_pinned_and_other_pins_restore_retention() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let working = store
        .create(draft(
            MemoryKind::Working,
            MemoryRetention::working(timestamp(2_000)),
        ))
        .unwrap();
    let error = store
        .set_pinned(working.id(), working.revision(), true, timestamp(1_100))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::Conflict);

    let invalid_retention = store
        .edit(
            working.id(),
            MemoryPatch::new(
                working.revision(),
                None,
                None,
                Some(MemoryRetention::UntilDeleted),
                timestamp(1_100),
                provenance(MemoryProvenanceKind::UserEdited, "memory-control", 1_100),
            )
            .unwrap(),
        )
        .unwrap_err();
    assert_eq!(invalid_retention.kind(), MemoryStoreErrorKind::Conflict);

    let episodic = store
        .create(draft(
            MemoryKind::Episodic,
            MemoryRetention::until(timestamp(9_000)),
        ))
        .unwrap();
    let pinned = store
        .set_pinned(episodic.id(), episodic.revision(), true, timestamp(1_100))
        .unwrap();
    assert!(pinned.pinned());
    assert_eq!(pinned.retention(), &MemoryRetention::UntilDeleted);
    assert_eq!(pinned.revision(), 2);

    let pinned_retention_edit = store
        .edit(
            pinned.id(),
            MemoryPatch::new(
                pinned.revision(),
                None,
                None,
                Some(MemoryRetention::until(timestamp(8_000))),
                timestamp(1_150),
                provenance(MemoryProvenanceKind::UserEdited, "memory-control", 1_150),
            )
            .unwrap(),
        )
        .unwrap_err();
    assert_eq!(pinned_retention_edit.kind(), MemoryStoreErrorKind::Conflict);

    let unpinned = store
        .set_pinned(pinned.id(), pinned.revision(), false, timestamp(1_200))
        .unwrap();
    assert!(!unpinned.pinned());
    assert_eq!(
        unpinned.retention(),
        &MemoryRetention::until(timestamp(9_000))
    );
    assert_eq!(unpinned.revision(), 3);
}

#[test]
fn expiry_is_exact_inspectable_and_session_scoped() {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    let working = store
        .create(draft(
            MemoryKind::Working,
            MemoryRetention::working(timestamp(2_000)),
        ))
        .unwrap();
    assert_eq!(
        store
            .inspect(working.id(), timestamp(1_999))
            .unwrap()
            .state(),
        MemoryState::Active
    );
    let expired = store.inspect(working.id(), timestamp(2_000)).unwrap();
    assert_eq!(expired.state(), MemoryState::Expired);
    assert_eq!(expired.revision(), 2);

    let due = store
        .create(draft(
            MemoryKind::Semantic,
            MemoryRetention::until(timestamp(2_500)),
        ))
        .unwrap();
    assert_eq!(store.prune_expired(timestamp(2_499)).unwrap(), 0);
    assert_eq!(store.prune_expired(timestamp(2_500)).unwrap(), 1);
    assert_eq!(
        store.inspect(due.id(), timestamp(2_500)).unwrap().state(),
        MemoryState::Expired
    );

    let session_id = SessionId::new(44);
    let session = store
        .create(draft(
            MemoryKind::Episodic,
            MemoryRetention::session(session_id),
        ))
        .unwrap();
    assert_eq!(
        store.expire_session(session_id, timestamp(3_000)).unwrap(),
        1
    );
    assert_eq!(
        store
            .inspect(session.id(), timestamp(3_000))
            .unwrap()
            .state(),
        MemoryState::Expired
    );
    assert_eq!(
        store.expire_session(session_id, timestamp(3_001)).unwrap(),
        0
    );
}
