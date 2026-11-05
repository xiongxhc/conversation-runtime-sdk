use conversation_protocol::PersonId;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{
    checked_timestamp, checked_unsigned, conflict_error, invalid_database_error, map_sqlite_error,
    not_found_error, sqlite_integer, store_error, SqliteMemoryStore,
};
use crate::{
    MemoryStoreErrorKind, MemoryStoreResult, NewPerson, PeopleStore, Person, PersonMaturity,
    PersonName, SignatureId, SignatureKey, SignatureModel, VoiceSignature, VoiceSignatureDraft,
    MAX_NAMES_PER_PERSON, MAX_PEOPLE,
};

impl PeopleStore for SqliteMemoryStore {
    fn people(&self) -> MemoryStoreResult<Vec<Person>> {
        let connection = self.connection()?;
        let ids = {
            let mut statement = connection
                .prepare("SELECT id FROM people ORDER BY id")
                .map_err(map_sqlite_error)?;
            let ids = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(map_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_error)?;
            ids
        };
        ids.into_iter()
            .map(|id| read_person(&connection, id))
            .collect()
    }

    fn person(&self, id: PersonId) -> MemoryStoreResult<Person> {
        let connection = self.connection()?;
        read_person(&connection, sqlite_integer(id.get())?)
    }

    fn people_named(&self, key: &str) -> MemoryStoreResult<Vec<PersonId>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT DISTINCT person_id FROM person_names WHERE name_key = ?1 ORDER BY person_id")
            .map_err(map_sqlite_error)?;
        let ids = statement
            .query_map([key], |row| row.get::<_, i64>(0))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        ids.into_iter().map(person_id).collect()
    }

    fn create_person(&self, person: NewPerson) -> MemoryStoreResult<Person> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let count: i64 = transaction
            .query_row("SELECT count(*) FROM people", [], |row| row.get(0))
            .map_err(map_sqlite_error)?;
        if count >= MAX_PEOPLE as i64 {
            return Err(store_error(
                MemoryStoreErrorKind::LimitExceeded,
                "memory store holds the maximum number of people",
            ));
        }
        transaction
            .execute(
                concat!(
                    "INSERT INTO people (is_owner, maturity, needs_review, created_at_ms) ",
                    "VALUES (0, ?1, ?2, ?3)"
                ),
                params![
                    PersonMaturity::Provisional.as_str(),
                    person.needs_review(),
                    person.created_at().get()
                ],
            )
            .map_err(map_sqlite_error)?;
        let id = transaction.last_insert_rowid();
        insert_name(&transaction, id, 0, person.name())?;
        let created = read_person(&transaction, id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(created)
    }

    fn add_name(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let id = sqlite_integer(id.get())?;
        require_person(&transaction, id)?;
        if has_name_key(&transaction, id, name.key())? {
            return Err(conflict_error());
        }
        let held: i64 = transaction
            .query_row(
                "SELECT count(*) FROM person_names WHERE person_id = ?1",
                [id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if held >= MAX_NAMES_PER_PERSON as i64 {
            return Err(store_error(
                MemoryStoreErrorKind::LimitExceeded,
                "person holds the maximum number of names",
            ));
        }
        let ordinal: i64 = transaction
            .query_row(
                "SELECT coalesce(max(ordinal) + 1, 0) FROM person_names WHERE person_id = ?1",
                [id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        insert_name(&transaction, id, ordinal, &name)?;
        let person = read_person(&transaction, id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(person)
    }

    fn rename(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let id = sqlite_integer(id.get())?;
        require_person(&transaction, id)?;
        transaction
            .execute(
                "DELETE FROM person_names WHERE person_id = ?1 AND name_key = ?2 AND ordinal != 0",
                params![id, name.key()],
            )
            .map_err(map_sqlite_error)?;
        transaction
            .execute(
                concat!(
                    "INSERT INTO person_names (person_id, ordinal, display, name_key) ",
                    "VALUES (?1, 0, ?2, ?3) ON CONFLICT (person_id, ordinal) ",
                    "DO UPDATE SET display = excluded.display, name_key = excluded.name_key"
                ),
                params![id, name.display(), name.key()],
            )
            .map_err(map_sqlite_error)?;
        transaction
            .execute("UPDATE people SET needs_review = 0 WHERE id = ?1", [id])
            .map_err(map_sqlite_error)?;
        let person = read_person(&transaction, id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(person)
    }

    fn mark_mature(&self, id: PersonId) -> MemoryStoreResult<Person> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let id = sqlite_integer(id.get())?;
        let changed = transaction
            .execute(
                "UPDATE people SET maturity = ?2 WHERE id = ?1",
                params![id, PersonMaturity::Mature.as_str()],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(not_found_error());
        }
        let person = read_person(&transaction, id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(person)
    }

    fn delete_person(&self, id: PersonId) -> MemoryStoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let id = sqlite_integer(id.get())?;
        if require_person(&transaction, id)? {
            return Err(conflict_error());
        }
        // The cascades remove names, voice signatures, and every memory about the person
        // with its sources and retrieval items.
        transaction
            .execute("DELETE FROM people WHERE id = ?1", [id])
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)
    }

    fn merge_people(&self, from: PersonId, into: PersonId) -> MemoryStoreResult<Person> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let from = sqlite_integer(from.get())?;
        let into = sqlite_integer(into.get())?;
        let from_is_owner = require_person(&transaction, from)?;
        require_person(&transaction, into)?;
        if from_is_owner || from == into {
            return Err(conflict_error());
        }
        transaction
            .execute(
                "UPDATE memories SET subject_person_id = ?2 WHERE subject_person_id = ?1",
                [from, into],
            )
            .map_err(map_sqlite_error)?;
        transaction
            .execute(
                "UPDATE voice_signatures SET person_id = ?2 WHERE person_id = ?1",
                [from, into],
            )
            .map_err(map_sqlite_error)?;
        for name in read_names(&transaction, from)? {
            if has_name_key(&transaction, into, name.key())? {
                continue;
            }
            let ordinal: i64 = transaction
                .query_row(
                    "SELECT coalesce(max(ordinal) + 1, 0) FROM person_names WHERE person_id = ?1",
                    [into],
                    |row| row.get(0),
                )
                .map_err(map_sqlite_error)?;
            insert_name(&transaction, into, ordinal, &name)?;
        }
        transaction
            .execute(
                concat!(
                    "UPDATE people SET needs_review = 0, maturity = CASE ",
                    "WHEN (SELECT maturity FROM people WHERE id = ?1) = 'mature' THEN 'mature' ",
                    "ELSE maturity END WHERE id = ?2"
                ),
                [from, into],
            )
            .map_err(map_sqlite_error)?;
        transaction
            .execute("DELETE FROM people WHERE id = ?1", [from])
            .map_err(map_sqlite_error)?;
        let person = read_person(&transaction, into)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(person)
    }

    fn verify_signature_key(&self, key: &SignatureKey) -> MemoryStoreResult<()> {
        self.verify_key(key)
    }

    fn add_signature(
        &self,
        key: &SignatureKey,
        person: PersonId,
        draft: VoiceSignatureDraft,
    ) -> MemoryStoreResult<VoiceSignature> {
        self.insert_signature(key, person, draft)
    }

    fn signatures(
        &self,
        key: &SignatureKey,
        model: &SignatureModel,
    ) -> MemoryStoreResult<Vec<VoiceSignature>> {
        self.read_signatures(key, model)
    }

    fn remove_signature(&self, id: SignatureId) -> MemoryStoreResult<()> {
        self.delete_signature(id)
    }

    fn reset_voice_signatures(&self) -> MemoryStoreResult<()> {
        self.delete_all_signatures()
    }
}

/// Returns whether the person is the owner, or `NotFound`.
fn require_person(connection: &Connection, id: i64) -> MemoryStoreResult<bool> {
    connection
        .query_row("SELECT is_owner FROM people WHERE id = ?1", [id], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(map_sqlite_error)?
        .map(|is_owner| is_owner == 1)
        .ok_or_else(not_found_error)
}

fn has_name_key(connection: &Connection, id: i64, key: &str) -> MemoryStoreResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM person_names WHERE person_id = ?1 AND name_key = ?2",
            params![id, key],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .is_some())
}

fn insert_name(
    connection: &Connection,
    id: i64,
    ordinal: i64,
    name: &PersonName,
) -> MemoryStoreResult<()> {
    connection
        .execute(
            "INSERT INTO person_names (person_id, ordinal, display, name_key) VALUES (?1, ?2, ?3, ?4)",
            params![id, ordinal, name.display(), name.key()],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn read_names(connection: &Connection, id: i64) -> MemoryStoreResult<Vec<PersonName>> {
    let mut statement = connection
        .prepare("SELECT display, name_key FROM person_names WHERE person_id = ?1 ORDER BY ordinal")
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite_error)?;
    rows.into_iter()
        .map(|(display, key)| PersonName::new(display, key).map_err(|_| invalid_database_error()))
        .collect()
}

fn read_person(connection: &Connection, id: i64) -> MemoryStoreResult<Person> {
    let (is_owner, maturity, needs_review, created_at) = connection
        .query_row(
            "SELECT is_owner, maturity, needs_review, created_at_ms FROM people WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_error)?
        .ok_or_else(not_found_error)?;
    let maturity = match maturity.as_str() {
        "provisional" => PersonMaturity::Provisional,
        "mature" => PersonMaturity::Mature,
        _ => return Err(invalid_database_error()),
    };
    Ok(Person::new(
        person_id(id)?,
        is_owner == 1,
        read_names(connection, id)?,
        maturity,
        needs_review == 1,
        checked_timestamp(created_at)?,
    ))
}

fn person_id(id: i64) -> MemoryStoreResult<PersonId> {
    PersonId::new(checked_unsigned(id)?).map_err(|_| invalid_database_error())
}
