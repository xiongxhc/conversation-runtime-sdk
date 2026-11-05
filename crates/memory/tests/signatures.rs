use std::path::{Path, PathBuf};

use conversation_memory::{
    MemoryStoreErrorKind, NewPerson, PeopleStore, PersonName, SignatureKey, SignatureModel,
    SignatureRole, SqliteMemoryStore, VoiceSignatureDraft, MAX_SIGNATURES_PER_PERSON,
};
use conversation_protocol::{PersonId, SessionId, UnixTimestampMillis};
use rusqlite::Connection;
use tempfile::{tempdir, TempDir};

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn key_a() -> SignatureKey {
    SignatureKey::from_hex(&"11".repeat(32)).unwrap()
}

fn key_b() -> SignatureKey {
    SignatureKey::from_hex(&"22".repeat(32)).unwrap()
}

fn model(id: &str, version: &str) -> SignatureModel {
    SignatureModel::new(id, version).unwrap()
}

fn ecapa() -> SignatureModel {
    model("ecapa", "1")
}

fn draft_for(model: SignatureModel, role: SignatureRole, embedding: &[f32]) -> VoiceSignatureDraft {
    VoiceSignatureDraft::new(
        model,
        role,
        embedding.to_vec(),
        1_800,
        timestamp(1_000),
        SessionId::new(7),
    )
    .unwrap()
}

fn draft(embedding: &[f32]) -> VoiceSignatureDraft {
    draft_for(ecapa(), SignatureRole::Enrolment, embedding)
}

fn store() -> (TempDir, PathBuf, SqliteMemoryStore) {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    (temporary, database, store)
}

fn person(store: &SqliteMemoryStore, display: &str) -> PersonId {
    store
        .create_person(NewPerson::new(
            PersonName::new(display, display.to_lowercase()).unwrap(),
            false,
            timestamp(1_000),
        ))
        .unwrap()
        .id()
}

fn raw(database: &Path) -> Connection {
    Connection::open(database).unwrap()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn signature_round_trips_and_is_not_stored_in_clear() {
    let (_temporary, database, store) = store();
    let jack = person(&store, "Jack");
    let stored = store
        .add_signature(&key_a(), jack, draft(&[0.25, -0.5, 1.0]))
        .unwrap();

    assert_eq!(stored.person(), jack);
    assert_eq!(stored.embedding(), &[0.25, -0.5, 1.0]);
    assert_eq!(stored.role(), SignatureRole::Enrolment);
    assert_eq!(stored.model(), &ecapa());
    assert_eq!(stored.speech_ms(), 1_800);
    assert_eq!(stored.created_at(), timestamp(1_000));
    assert_eq!(stored.source_session(), SessionId::new(7));
    assert_eq!(store.signatures(&key_a(), &ecapa()).unwrap(), vec![stored]);
    let blob: Vec<u8> = raw(&database)
        .query_row("SELECT ciphertext FROM voice_signatures", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!contains_subslice(&blob, &0.25_f32.to_le_bytes()));
    assert!(!contains_subslice(&blob, &(-0.5_f32).to_le_bytes()));
}

#[test]
fn wrong_key_is_refused_rather_than_reading_empty() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();

    let mismatch = MemoryStoreErrorKind::KeyMismatch;
    assert_eq!(
        store.verify_signature_key(&key_b()).unwrap_err().kind(),
        mismatch
    );
    assert_eq!(
        store.signatures(&key_b(), &ecapa()).unwrap_err().kind(),
        mismatch
    );
    assert_eq!(
        store
            .add_signature(&key_b(), jack, draft(&[1.0]))
            .unwrap_err()
            .kind(),
        mismatch
    );
    assert_eq!(store.signatures(&key_a(), &ecapa()).unwrap().len(), 1);
}

#[test]
fn first_key_used_is_the_only_key_accepted() {
    let (_temporary, _database, store) = store();
    store.verify_signature_key(&key_a()).unwrap();
    store.verify_signature_key(&key_a()).unwrap();
    assert_eq!(
        store.verify_signature_key(&key_b()).unwrap_err().kind(),
        MemoryStoreErrorKind::KeyMismatch
    );
}

#[test]
fn malformed_key_material_is_unavailable() {
    let too_short = "a".repeat(63);
    let not_hex = "g".repeat(64);
    let too_long = "a".repeat(65);
    for material in [
        "",
        "abc",
        too_short.as_str(),
        not_hex.as_str(),
        too_long.as_str(),
    ] {
        assert_eq!(
            SignatureKey::from_hex(material).unwrap_err().kind(),
            MemoryStoreErrorKind::KeyUnavailable
        );
    }
}

#[test]
fn key_material_is_read_like_the_recording_key() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    let upper = SignatureKey::from_hex(&format!(" {} ", "AB".repeat(32))).unwrap();
    let lower = SignatureKey::from_hex(&"ab".repeat(32)).unwrap();
    store.add_signature(&upper, jack, draft(&[1.0])).unwrap();
    assert_eq!(store.signatures(&lower, &ecapa()).unwrap().len(), 1);
}

#[test]
fn missing_or_misnamed_key_variable_is_unavailable() {
    for name in [
        "CONVERSATION_RUNTIME_TEST_UNSET_SIGNATURE_KEY",
        "lower_case",
        "1STARTS_WITH_DIGIT",
        "",
    ] {
        assert_eq!(
            SignatureKey::from_environment(name).unwrap_err().kind(),
            MemoryStoreErrorKind::KeyUnavailable
        );
    }
}

#[test]
fn other_models_signatures_are_never_returned() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    let ecapa_one = store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();
    store
        .add_signature(
            &key_a(),
            jack,
            draft_for(model("eres2net", "1"), SignatureRole::Enrolment, &[2.0]),
        )
        .unwrap();
    store
        .add_signature(
            &key_a(),
            jack,
            draft_for(model("ecapa", "2"), SignatureRole::Enrolment, &[3.0]),
        )
        .unwrap();
    assert_eq!(
        store.signatures(&key_a(), &ecapa()).unwrap(),
        vec![ecapa_one]
    );
}

#[test]
fn signatures_list_every_person_ordered_by_person_then_id() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    let jack_first = store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();
    let owner = store
        .add_signature(&key_a(), PersonId::OWNER, draft(&[2.0]))
        .unwrap();
    let jack_second = store.add_signature(&key_a(), jack, draft(&[3.0])).unwrap();
    let ids: Vec<_> = store
        .signatures(&key_a(), &ecapa())
        .unwrap()
        .iter()
        .map(|signature| signature.id())
        .collect();
    assert_eq!(ids, vec![owner.id(), jack_first.id(), jack_second.id()]);
}

#[test]
fn tampered_ciphertext_is_an_invalid_database() {
    let (_temporary, database, store) = store();
    let jack = person(&store, "Jack");
    store
        .add_signature(&key_a(), jack, draft(&[1.0, 2.0]))
        .unwrap();
    raw(&database)
        .execute(
            "UPDATE voice_signatures SET ciphertext = CAST(zeroblob(24) AS BLOB)",
            [],
        )
        .unwrap();
    assert_eq!(
        store.signatures(&key_a(), &ecapa()).unwrap_err().kind(),
        MemoryStoreErrorKind::InvalidDatabase
    );
}

#[test]
fn tampered_metadata_is_an_invalid_database() {
    let (_temporary, database, store) = store();
    let jack = person(&store, "Jack");
    store
        .add_signature(
            &key_a(),
            jack,
            draft_for(ecapa(), SignatureRole::Refinement, &[1.0]),
        )
        .unwrap();
    raw(&database)
        .execute("UPDATE voice_signatures SET role = 'enrolment'", [])
        .unwrap();
    assert_eq!(
        store.signatures(&key_a(), &ecapa()).unwrap_err().kind(),
        MemoryStoreErrorKind::InvalidDatabase
    );
}

#[test]
fn only_refinement_samples_can_be_removed() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    let anchor = store
        .add_signature(
            &key_a(),
            jack,
            draft_for(ecapa(), SignatureRole::Anchor, &[1.0]),
        )
        .unwrap();
    let enrolment = store.add_signature(&key_a(), jack, draft(&[2.0])).unwrap();
    let refinement = store
        .add_signature(
            &key_a(),
            jack,
            draft_for(ecapa(), SignatureRole::Refinement, &[3.0]),
        )
        .unwrap();

    for immutable in [anchor.id(), enrolment.id()] {
        assert_eq!(
            store.remove_signature(immutable).unwrap_err().kind(),
            MemoryStoreErrorKind::Conflict
        );
    }
    store.remove_signature(refinement.id()).unwrap();

    assert_eq!(
        store.signatures(&key_a(), &ecapa()).unwrap(),
        vec![anchor, enrolment]
    );
    assert_eq!(
        store.remove_signature(refinement.id()).unwrap_err().kind(),
        MemoryStoreErrorKind::NotFound
    );
}

#[test]
fn non_finite_or_empty_embeddings_are_refused() {
    let too_large = vec![0.5; 4_097];
    for embedding in [
        &[] as &[f32],
        &[f32::NAN],
        &[1.0, f32::INFINITY],
        too_large.as_slice(),
    ] {
        let error = VoiceSignatureDraft::new(
            ecapa(),
            SignatureRole::Enrolment,
            embedding.to_vec(),
            1_000,
            timestamp(1_000),
            SessionId::new(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), MemoryStoreErrorKind::Conflict);
    }
    assert!(SignatureModel::new("", "1").is_err());
    assert!(SignatureModel::new("ecapa", "v".repeat(129)).is_err());
}

#[test]
fn signature_for_an_unknown_person_is_refused() {
    let (_temporary, _database, store) = store();
    let error = store
        .add_signature(&key_a(), PersonId::new(42).unwrap(), draft(&[1.0]))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::NotFound);
}

#[test]
fn signatures_per_person_are_bounded() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    for _ in 0..MAX_SIGNATURES_PER_PERSON {
        store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();
    }
    let error = store
        .add_signature(&key_a(), jack, draft(&[1.0]))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::LimitExceeded);
    let other = person(&store, "Chris");
    store.add_signature(&key_a(), other, draft(&[1.0])).unwrap();
}

#[test]
fn deleting_a_person_deletes_their_signatures() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();
    let owner = store
        .add_signature(&key_a(), PersonId::OWNER, draft(&[2.0]))
        .unwrap();
    store.delete_person(jack).unwrap();
    assert_eq!(store.signatures(&key_a(), &ecapa()).unwrap(), vec![owner]);
}

#[test]
fn merging_moves_signatures_to_the_surviving_person() {
    let (_temporary, _database, store) = store();
    let jack = person(&store, "Jack");
    let duplicate = person(&store, "Jack");
    store
        .add_signature(&key_a(), duplicate, draft(&[1.0]))
        .unwrap();
    store.merge_people(duplicate, jack).unwrap();
    let signatures = store.signatures(&key_a(), &ecapa()).unwrap();
    assert_eq!(signatures.len(), 1);
    assert_eq!(signatures[0].person(), jack);
    assert_eq!(signatures[0].embedding(), &[1.0]);
}

#[test]
fn key_debug_output_hides_the_key() {
    assert_eq!(format!("{:?}", key_a()), "SignatureKey(..)");
}
