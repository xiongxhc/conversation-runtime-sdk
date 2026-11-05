use std::path::PathBuf;

use conversation_memory::{
    MemoryStore, MemoryStoreErrorKind, NewPerson, PeopleStore, PersonMaturity, PersonName,
    SqliteMemoryStore, MAX_NAMES_PER_PERSON, MAX_PEOPLE,
};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryKind, MemoryProvenance, MemoryProvenanceKind,
    MemoryRetention, PersonId, UnixTimestampMillis,
};
use tempfile::{tempdir, TempDir};

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn name(display: &str, key: &str) -> PersonName {
    PersonName::new(display, key).unwrap()
}

fn new_person(display: &str, key: &str, needs_review: bool) -> NewPerson {
    NewPerson::new(name(display, key), needs_review, timestamp(1_000))
}

fn draft(content: &str, subject: PersonId) -> MemoryDraft {
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
    .with_subject(subject)
}

fn store() -> (TempDir, PathBuf, SqliteMemoryStore) {
    let temporary = tempdir().unwrap();
    let database = temporary.path().join("runtime.sqlite3");
    let store = SqliteMemoryStore::initialize(&database).unwrap();
    (temporary, database, store)
}

fn names(store: &SqliteMemoryStore, id: PersonId) -> Vec<String> {
    store
        .person(id)
        .unwrap()
        .names()
        .iter()
        .map(|name| name.display().to_owned())
        .collect()
}

#[test]
fn created_person_is_provisional_and_findable_by_key() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    assert_eq!(jack.maturity(), PersonMaturity::Provisional);
    assert!(!jack.is_owner());
    assert!(!jack.needs_review());
    assert_eq!(jack.display_name(), Some("Jack"));
    assert_eq!(jack.created_at(), timestamp(1_000));
    assert_eq!(store.people_named("jack").unwrap(), vec![jack.id()]);
    assert_eq!(store.person(jack.id()).unwrap(), jack);
}

#[test]
fn people_lists_the_owner_first_then_everyone_by_id() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    let ids: Vec<PersonId> = store.people().unwrap().iter().map(|p| p.id()).collect();
    assert_eq!(ids, vec![PersonId::OWNER, jack.id()]);
}

#[test]
fn two_people_may_share_a_name() {
    let (_temporary, _database, store) = store();
    let first = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    let second = store
        .create_person(new_person("Jack", "jack", true))
        .unwrap();
    assert!(second.needs_review());
    assert_eq!(
        store.people_named("jack").unwrap(),
        vec![first.id(), second.id()]
    );
}

#[test]
fn alias_is_found_and_duplicate_alias_is_refused() {
    let (_temporary, _database, store) = store();
    let chris = store
        .create_person(new_person("Chris", "chris", false))
        .unwrap();
    let chris = store.add_name(chris.id(), name("昊辰", "haochen")).unwrap();
    assert_eq!(names(&store, chris.id()), vec!["Chris", "昊辰"]);
    assert_eq!(store.people_named("haochen").unwrap(), vec![chris.id()]);
    let error = store
        .add_name(chris.id(), name("Haochen", "haochen"))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::Conflict);
}

#[test]
fn owner_starts_unnamed_and_rename_sets_the_name() {
    let (_temporary, _database, store) = store();
    let owner = store.person(PersonId::OWNER).unwrap();
    assert!(owner.is_owner());
    assert!(owner.display_name().is_none());
    assert!(owner.names().is_empty());
    let owner = store
        .rename(PersonId::OWNER, name("Chris", "chris"))
        .unwrap();
    assert_eq!(owner.display_name(), Some("Chris"));
    assert_eq!(store.people_named("chris").unwrap(), vec![PersonId::OWNER]);
}

#[test]
fn rename_clears_needs_review_and_absorbs_a_matching_alias() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", true))
        .unwrap();
    store.add_name(jack.id(), name("Jake", "jake")).unwrap();
    let renamed = store.rename(jack.id(), name("Jake", "jake")).unwrap();
    assert!(!renamed.needs_review());
    assert_eq!(names(&store, jack.id()), vec!["Jake"]);
    assert!(store.people_named("jack").unwrap().is_empty());
}

#[test]
fn mark_mature_changes_only_maturity() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    let mature = store.mark_mature(jack.id()).unwrap();
    assert_eq!(mature.maturity(), PersonMaturity::Mature);
    assert_eq!(mature.names(), jack.names());
}

#[test]
fn unknown_people_are_not_found() {
    let (_temporary, _database, store) = store();
    let ghost = PersonId::new(42).unwrap();
    let not_found = MemoryStoreErrorKind::NotFound;
    assert_eq!(store.person(ghost).unwrap_err().kind(), not_found);
    assert_eq!(
        store
            .add_name(ghost, name("Ghost", "ghost"))
            .unwrap_err()
            .kind(),
        not_found
    );
    assert_eq!(
        store
            .rename(ghost, name("Ghost", "ghost"))
            .unwrap_err()
            .kind(),
        not_found
    );
    assert_eq!(store.mark_mature(ghost).unwrap_err().kind(), not_found);
    assert_eq!(store.delete_person(ghost).unwrap_err().kind(), not_found);
    assert_eq!(
        store
            .merge_people(ghost, PersonId::OWNER)
            .unwrap_err()
            .kind(),
        not_found
    );
    assert_eq!(
        store
            .merge_people(
                store
                    .create_person(new_person("Jack", "jack", false))
                    .unwrap()
                    .id(),
                ghost
            )
            .unwrap_err()
            .kind(),
        not_found
    );
}

#[test]
fn deleting_a_person_deletes_only_their_memories() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    let jacks = store.create(draft("Jack likes tea", jack.id())).unwrap();
    let owners = store
        .create(draft("The owner likes coffee", PersonId::OWNER))
        .unwrap();

    store.delete_person(jack.id()).unwrap();

    let remaining: Vec<_> = store
        .list(timestamp(2_000))
        .unwrap()
        .iter()
        .map(|record| record.id())
        .collect();
    assert_eq!(remaining, vec![owners.id()]);
    assert_eq!(
        store
            .inspect(jacks.id(), timestamp(2_000))
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::NotFound
    );
    assert!(store.people_named("jack").unwrap().is_empty());
    assert_eq!(
        store.person(jack.id()).unwrap_err().kind(),
        MemoryStoreErrorKind::NotFound
    );
}

#[test]
fn owner_cannot_be_deleted_or_merged_away() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    assert_eq!(
        store.delete_person(PersonId::OWNER).unwrap_err().kind(),
        MemoryStoreErrorKind::Conflict
    );
    assert_eq!(
        store
            .merge_people(PersonId::OWNER, jack.id())
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::Conflict
    );
    assert_eq!(
        store.merge_people(jack.id(), jack.id()).unwrap_err().kind(),
        MemoryStoreErrorKind::Conflict
    );
    assert!(store.person(PersonId::OWNER).unwrap().is_owner());
}

#[test]
fn merge_moves_memories_and_names_and_removes_the_source() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    let duplicate = store
        .create_person(new_person("Jack", "jack", true))
        .unwrap();
    store
        .add_name(duplicate.id(), name("Jake", "jake"))
        .unwrap();
    store.mark_mature(duplicate.id()).unwrap();
    let memory = store
        .create(draft("Jack likes tea", duplicate.id()))
        .unwrap();

    let merged = store.merge_people(duplicate.id(), jack.id()).unwrap();

    assert_eq!(merged.id(), jack.id());
    assert_eq!(names(&store, jack.id()), vec!["Jack", "Jake"]);
    assert_eq!(merged.maturity(), PersonMaturity::Mature);
    assert!(!merged.needs_review());
    assert_eq!(
        store
            .inspect(memory.id(), timestamp(2_000))
            .unwrap()
            .subject(),
        jack.id()
    );
    assert_eq!(
        store.person(duplicate.id()).unwrap_err().kind(),
        MemoryStoreErrorKind::NotFound
    );
    assert_eq!(store.people_named("jake").unwrap(), vec![jack.id()]);
}

#[test]
fn merging_a_duplicate_into_the_owner_is_allowed() {
    let (_temporary, _database, store) = store();
    store
        .rename(PersonId::OWNER, name("Chris", "chris"))
        .unwrap();
    let duplicate = store
        .create_person(new_person("Chris", "chris", true))
        .unwrap();
    let memory = store
        .create(draft("Chris likes tea", duplicate.id()))
        .unwrap();

    let owner = store.merge_people(duplicate.id(), PersonId::OWNER).unwrap();

    assert!(owner.is_owner());
    assert_eq!(names(&store, PersonId::OWNER), vec!["Chris"]);
    assert_eq!(
        store
            .inspect(memory.id(), timestamp(2_000))
            .unwrap()
            .subject(),
        PersonId::OWNER
    );
}

#[test]
fn person_count_is_bounded() {
    let (_temporary, _database, store) = store();
    for index in 1..MAX_PEOPLE {
        store
            .create_person(new_person("Guest", &format!("guest-{index}"), false))
            .unwrap();
    }
    assert_eq!(store.people().unwrap().len(), MAX_PEOPLE);
    let error = store
        .create_person(new_person("One more", "one-more", false))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::LimitExceeded);
}

#[test]
fn blank_or_oversized_names_are_refused() {
    for (display, key) in [
        ("  ", "key"),
        ("Jack", ""),
        ("Jack", " \t "),
        (&"x".repeat(129) as &str, "key"),
        ("Jack", &"k".repeat(129) as &str),
    ] {
        assert_eq!(
            PersonName::new(display, key).unwrap_err().kind(),
            MemoryStoreErrorKind::Conflict
        );
    }
    assert!(PersonName::new("x".repeat(128), "k".repeat(128)).is_ok());
}

#[test]
fn names_per_person_are_bounded() {
    let (_temporary, _database, store) = store();
    let jack = store
        .create_person(new_person("Jack", "jack", false))
        .unwrap();
    for index in 1..MAX_NAMES_PER_PERSON {
        store
            .add_name(jack.id(), name("Alias", &format!("alias-{index}")))
            .unwrap();
    }
    assert_eq!(
        store.person(jack.id()).unwrap().names().len(),
        MAX_NAMES_PER_PERSON
    );
    let error = store
        .add_name(jack.id(), name("One more", "one-more"))
        .unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::LimitExceeded);
    let renamed = store.rename(jack.id(), name("Jake", "jake")).unwrap();
    assert_eq!(renamed.names().len(), MAX_NAMES_PER_PERSON);
}
