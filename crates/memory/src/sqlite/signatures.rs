use conversation_protocol::{PersonId, SessionId};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{
    checked_timestamp, checked_unsigned, conflict_error, invalid_database_error, map_sqlite_error,
    not_found_error, sqlite_integer, store_error, SqliteMemoryStore,
};
use crate::signatures::{key_mismatch_error, signature_aad, KEY_CHECK_AAD, KEY_CHECK_PLAINTEXT};
use crate::{
    MemoryStoreErrorKind, MemoryStoreResult, SignatureId, SignatureKey, SignatureModel,
    SignatureRole, VoiceSignature, VoiceSignatureDraft, MAX_SIGNATURES_PER_PERSON,
};

impl SqliteMemoryStore {
    pub(super) fn verify_key(&self, key: &SignatureKey) -> MemoryStoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = immediate(&mut connection)?;
        check_key(&transaction, key)?;
        transaction.commit().map_err(map_sqlite_error)
    }

    pub(super) fn insert_signature(
        &self,
        key: &SignatureKey,
        person: PersonId,
        draft: VoiceSignatureDraft,
    ) -> MemoryStoreResult<VoiceSignature> {
        let mut connection = self.connection()?;
        let transaction = immediate(&mut connection)?;
        check_key(&transaction, key)?;
        let person_id = sqlite_integer(person.get())?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM people WHERE id = ?1",
                [person_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        if !exists {
            return Err(not_found_error());
        }
        let held: i64 = transaction
            .query_row(
                "SELECT count(*) FROM voice_signatures WHERE person_id = ?1",
                [person_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if held >= MAX_SIGNATURES_PER_PERSON as i64 {
            return Err(store_error(
                MemoryStoreErrorKind::LimitExceeded,
                "person holds the maximum number of voice signatures",
            ));
        }
        let dimension = u32::try_from(draft.embedding().len()).map_err(|_| conflict_error())?;
        let plaintext: Vec<u8> = draft
            .embedding()
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let (nonce, ciphertext) = key.seal(
            &signature_aad(draft.model(), draft.role(), dimension),
            &plaintext,
        )?;
        transaction
            .execute(
                concat!(
                    "INSERT INTO voice_signatures (person_id, model_id, model_version, role, ",
                    "dimension, speech_ms, created_at_ms, source_session_id, nonce, ciphertext) ",
                    "VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                ),
                params![
                    person_id,
                    draft.model().id(),
                    draft.model().version(),
                    draft.role().as_str(),
                    dimension,
                    draft.speech_ms(),
                    draft.created_at().get(),
                    sqlite_integer(draft.source_session().get())?,
                    nonce.as_slice(),
                    ciphertext,
                ],
            )
            .map_err(map_sqlite_error)?;
        let id = SignatureId::new(checked_unsigned(transaction.last_insert_rowid())?);
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(VoiceSignature::new(id, person, draft))
    }

    pub(super) fn read_signatures(
        &self,
        key: &SignatureKey,
        model: &SignatureModel,
    ) -> MemoryStoreResult<Vec<VoiceSignature>> {
        let mut connection = self.connection()?;
        let transaction = immediate(&mut connection)?;
        check_key(&transaction, key)?;
        let rows = {
            let mut statement = transaction
                .prepare(concat!(
                    "SELECT id, person_id, role, dimension, speech_ms, created_at_ms, ",
                    "source_session_id, nonce, ciphertext FROM voice_signatures ",
                    "WHERE model_id = ?1 AND model_version = ?2 ORDER BY person_id, id"
                ))
                .map_err(map_sqlite_error)?;
            let rows = statement
                .query_map(params![model.id(), model.version()], |row| {
                    Ok(SignatureRow {
                        id: row.get(0)?,
                        person: row.get(1)?,
                        role: row.get(2)?,
                        dimension: row.get(3)?,
                        speech_ms: row.get(4)?,
                        created_at: row.get(5)?,
                        source_session: row.get(6)?,
                        nonce: row.get(7)?,
                        ciphertext: row.get(8)?,
                    })
                })
                .map_err(map_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_error)?;
            rows
        };
        let signatures = rows
            .into_iter()
            .map(|row| row.decrypt(key, model))
            .collect::<MemoryStoreResult<Vec<_>>>()?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(signatures)
    }

    pub(super) fn delete_all_signatures(&self) -> MemoryStoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = immediate(&mut connection)?;
        transaction
            .execute_batch("DELETE FROM voice_signatures; DELETE FROM signature_key_check;")
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)
    }

    pub(super) fn delete_signature(&self, id: SignatureId) -> MemoryStoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = immediate(&mut connection)?;
        let id = sqlite_integer(id.get())?;
        let role = transaction
            .query_row(
                "SELECT role FROM voice_signatures WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(not_found_error)?;
        if SignatureRole::parse(&role) != Some(SignatureRole::Refinement) {
            return Err(conflict_error());
        }
        transaction
            .execute("DELETE FROM voice_signatures WHERE id = ?1", [id])
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)
    }
}

struct SignatureRow {
    id: i64,
    person: i64,
    role: String,
    dimension: i64,
    speech_ms: i64,
    created_at: i64,
    source_session: i64,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

impl SignatureRow {
    fn decrypt(
        self,
        key: &SignatureKey,
        model: &SignatureModel,
    ) -> MemoryStoreResult<VoiceSignature> {
        let role = SignatureRole::parse(&self.role).ok_or_else(invalid_database_error)?;
        let dimension = u32::try_from(self.dimension).map_err(|_| invalid_database_error())?;
        let nonce: [u8; 12] = self
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| invalid_database_error())?;
        // The key check already passed, so a row that does not open was altered.
        let plaintext = key
            .open(
                nonce,
                &signature_aad(model, role, dimension),
                &self.ciphertext,
            )
            .ok_or_else(invalid_database_error)?;
        if plaintext.len() != dimension as usize * 4 {
            return Err(invalid_database_error());
        }
        let embedding = plaintext
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect();
        let draft = VoiceSignatureDraft::new(
            model.clone(),
            role,
            embedding,
            u32::try_from(self.speech_ms).map_err(|_| invalid_database_error())?,
            checked_timestamp(self.created_at)?,
            SessionId::new(checked_unsigned(self.source_session)?),
        )
        .map_err(|_| invalid_database_error())?;
        let person =
            PersonId::new(checked_unsigned(self.person)?).map_err(|_| invalid_database_error())?;
        Ok(VoiceSignature::new(
            SignatureId::new(checked_unsigned(self.id)?),
            person,
            draft,
        ))
    }
}

fn immediate(connection: &mut Connection) -> MemoryStoreResult<Transaction<'_>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)
}

/// The first key used is recorded by encrypting a fixed value; every later use must
/// decrypt it. A different key therefore fails instead of reading as an empty store.
fn check_key(connection: &Connection, key: &SignatureKey) -> MemoryStoreResult<()> {
    let stored = connection
        .query_row(
            "SELECT nonce, ciphertext FROM signature_key_check WHERE id = 1",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    let Some((nonce, ciphertext)) = stored else {
        let (nonce, ciphertext) = key.seal(KEY_CHECK_AAD, KEY_CHECK_PLAINTEXT)?;
        connection
            .execute(
                "INSERT INTO signature_key_check (id, nonce, ciphertext) VALUES (1, ?1, ?2)",
                params![nonce.as_slice(), ciphertext],
            )
            .map_err(map_sqlite_error)?;
        return Ok(());
    };
    let nonce: [u8; 12] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| invalid_database_error())?;
    match key.open(nonce, KEY_CHECK_AAD, &ciphertext) {
        Some(plaintext) if plaintext == KEY_CHECK_PLAINTEXT => Ok(()),
        _ => Err(key_mismatch_error()),
    }
}
