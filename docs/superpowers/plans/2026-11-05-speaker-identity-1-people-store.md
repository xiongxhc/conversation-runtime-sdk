# Speaker Identity, Plan 1: People Store and Memory Subject Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the local memory store people (with names, aliases and an owner),
encrypted voice signatures, and a subject person on every memory, upgrading
existing version 1 databases in place.

**Architecture:** Everything lives in `crates/memory` inside the existing SQLite
database, so deleting or merging a person and their memories is one
transaction. Schema version 2 adds `people`, `person_names`, `voice_signatures`
and `signature_key_check`, and rebuilds `memories` with a required
`subject_person_id`. The owner is always person 1. Signatures are AES-256-GCM
encrypted with their own key. No wire type, gateway or SDK change is in this
plan: `ClientMemoryRecord` stays as it is.

**Tech Stack:** Rust, rusqlite 0.40 (bundled SQLite), ring 0.17 (AEAD, random).

**Spec:** `docs/superpowers/specs/2026-11-03-voice-speaker-identity-design.md`

**Series.** The spec is built in six plans, each shippable on its own:

1. People store and memory subject (this plan).
2. Identity engine: matching, claim outcomes, enrolment maturation, pending
   identity question, name-claim detection and name normalisation, against
   fixture embeddings.
3. Embedding model: measurement of ECAPA against ERes2Net (and CAM++) on Apple
   Silicon, and the chosen model's Rust runtime.
4. Live path: `[speaker_identity]` gateway configuration and key, sidecar
   protocol version forwarding turn audio and the playback-active flag,
   media-bridge audio, latency budget, speaker labels in conversation context,
   subject-scoped retrieval and extraction, v4 voice events with SDK types.
5. People management: SDK commands (list, rename, merge, delete, "this is me")
   and the desktop People screen.
6. Evaluation report, `docs/architecture.md` and `docs/HUMAN-CHECKLIST.md`
   amendments, and the native human check.

## Global Constraints

- Traces, logs and error messages are content-free: no names, transcript text,
  signatures or audio. Store errors stay `&'static str` messages.
- Store voice signatures (embedding, model identifier and version, creation
  time, source conversation id), never audio, encrypted at rest with their own
  key.
- The key format matches the recording key: a 256-bit key written as 64
  lowercase hexadecimal characters.
- A missing or wrong key must fail; it never yields an empty signature list.
- Signatures from different models are never compared: every signature read is
  filtered by model identifier and version.
- Every memory stored before this feature belongs to the owner person; no path
  creates a memory without a subject.
- Deleting a person deletes their voice signatures and every memory whose
  subject is that person. Merging two people moves the signatures and memories
  of one onto the other.
- Refuse an unknown future schema version (`UnsupportedSchema`).
- Gates: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`.

## Review Focus

1. **A real version 1 database with data upgrades losslessly.** Every memory,
   source row, approval, pinned flag, retention and revision survives, all
   subjects become the owner, and a memory created after the upgrade gets an id
   above the old maximum (AUTOINCREMENT carried over). Test:
   `upgrade_preserves_every_record_and_keeps_ids_increasing` in Task 1.
2. **A failed upgrade leaves version 1 untouched.** A version 1 file with a bad
   checksum row is refused and still reads `user_version = 1` afterwards. Test:
   `invalid_version_1_database_is_refused_and_left_unchanged` in Task 1.
3. **The wrong key never reads as "no signatures".** With signatures stored,
   opening with another key returns `KeyMismatch` from both
   `verify_signature_key` and `signatures`. Test:
   `wrong_key_is_refused_rather_than_reading_empty` in Task 4.
4. **Subject-scoped retrieval never leaks another person's pinned memory.** A
   pinned memory of the owner that matches the query is not returned when
   retrieval is scoped to Jack. Test:
   `subject_scope_excludes_other_peoples_pinned_matches` in Task 2.
5. **A room full of name claims cannot grow storage without bound.** Creating
   person 257 returns `LimitExceeded`; signature 65 for one person returns
   `LimitExceeded`. Tests: `person_count_is_bounded` in Task 3,
   `signatures_per_person_are_bounded` in Task 4.

---

### Task 1: Schema version 2 and in-place upgrade

**Files:**
- Create: `crates/memory/migrations/0002_people.sql`
- Create: `crates/memory/tests/fixtures/memory-v1.sqlite3` (binary, generated in Step 1)
- Create: `crates/memory/tests/schema_upgrade.rs`
- Modify: `crates/protocol/src/memory.rs` (add `PersonId`), `crates/protocol/src/lib.rs` (export it)
- Modify: `crates/memory/src/sqlite.rs` (`SCHEMA_VERSION`, `initialize_database`, `open`, `validate_database`, `create`)
- Modify: `crates/memory/tests/sqlite_lifecycle.rs:34` (expects 2) and `:76` (the "newer" case sets 3)

**Interfaces:**
- Produces: `conversation_protocol::PersonId` (via `checked_id!(PersonId, "person identifier")`, so `PersonId::new(u64) -> Result<PersonId, RuntimeError>`, `get() -> u64`) and `pub const OWNER: PersonId` as an associated constant equal to 1.
- Produces: `conversation_memory::SCHEMA_VERSION == 2`.

- [ ] **Step 1: Generate the version 1 fixture from the current code, before any change**

On the unchanged tree, add a scratch test `crates/memory/tests/make_v1_fixture.rs`
marked `#[ignore]` that calls `SqliteMemoryStore::initialize` on
`tests/fixtures/memory-v1.sqlite3` and creates, through the public API:
memory A (semantic, `UntilDeleted`), memory B (semantic, `Until` an expiry in
the far future, then `set_pinned(true)`), memory C (identity, starts as
candidate, then `approve`d), memory D (episodic, `Session` retention) and then
deletes D (so the AUTOINCREMENT high-water mark, 4, sits above the highest
surviving id, 3). Run it, then fold the WAL into the file and remove the
scratch test:

Run: `cargo test -p conversation-memory --test make_v1_fixture -- --ignored && sqlite3 crates/memory/tests/fixtures/memory-v1.sqlite3 "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;" && rm crates/memory/tests/make_v1_fixture.rs crates/memory/tests/fixtures/memory-v1.sqlite3-*`
Expected: the fixture exists alone; `sqlite3 … "PRAGMA user_version"` prints `1`.

- [ ] **Step 2: Write the failing upgrade tests in `crates/memory/tests/schema_upgrade.rs`**

Each test copies the fixture into a `tempdir()` and sets mode `0o600` first
(`validate_file_permissions` refuses group or other access).

```rust
#[test]
fn upgrade_preserves_every_record_and_keeps_ids_increasing() {
    let database = copy_fixture();
    let before = read_v1_rows(&database); // raw rusqlite: every memories and memory_sources row
    let store = SqliteMemoryStore::open(&database).unwrap();
    assert_eq!(pragma_user_version(&database), 2);
    let after = read_rows_without_subject(&database);
    assert_eq!(after, before);
    assert_eq!(distinct_subjects(&database), vec![1]); // SELECT DISTINCT subject_person_id FROM memories
    let created = store.create(draft("new after upgrade")).unwrap();
    assert_eq!(created.id().get(), 5);
}

#[test]
fn fresh_database_and_upgraded_database_have_the_same_schema() {
    // compare `SELECT type, name, sql FROM sqlite_schema ORDER BY name`
    // between SqliteMemoryStore::initialize(new) and open(copy_fixture())
}

#[test]
fn owner_person_exists_after_upgrade_and_after_initialize() {
    // SELECT id, is_owner FROM people == [(1, 1)] in both cases
}

#[test]
fn invalid_version_1_database_is_refused_and_left_unchanged() {
    let database = copy_fixture();
    raw(&database).execute("UPDATE schema_migrations SET checksum = '0000000000000000'", []).unwrap();
    assert_eq!(SqliteMemoryStore::open(&database).unwrap_err().kind(), MemoryStoreErrorKind::InvalidDatabase);
    assert_eq!(pragma_user_version(&database), 1);
    assert!(!table_exists(&database, "people"));
}

#[test]
fn unknown_future_version_is_refused() {
    // initialize, set user_version = 3 → UnsupportedSchema
}
```

`created.id().get() == 5` relies on the fixture's deleted memory D: a rebuild
that loses the `sqlite_sequence` high-water mark would hand out 4, reusing an id
a client may still hold.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p conversation-memory --test schema_upgrade`
Expected: FAIL (`user_version` is 1 after `open`, `people` does not exist).

- [ ] **Step 4: Write `crates/memory/migrations/0002_people.sql`**

The schema is a decision, so it is fixed here:

```sql
CREATE TABLE schema_migrations_next (
    version INTEGER PRIMARY KEY CHECK (version IN (1, 2)),
    checksum TEXT NOT NULL CHECK (length(checksum) = 16)
) STRICT;
INSERT INTO schema_migrations_next (version, checksum) SELECT version, checksum FROM schema_migrations;
DROP TABLE schema_migrations;
ALTER TABLE schema_migrations_next RENAME TO schema_migrations;

CREATE TABLE people (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    is_owner INTEGER NOT NULL CHECK (is_owner IN (0, 1)),
    maturity TEXT NOT NULL CHECK (maturity IN ('provisional', 'mature')),
    needs_review INTEGER NOT NULL DEFAULT 0 CHECK (needs_review IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    CHECK (is_owner = 0 OR (id = 1 AND needs_review = 0))
) STRICT;
CREATE UNIQUE INDEX people_single_owner ON people(is_owner) WHERE is_owner = 1;
INSERT INTO people (id, is_owner, maturity, created_at_ms) VALUES (1, 1, 'provisional', 0);

CREATE TABLE person_names (
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    display TEXT NOT NULL CHECK (length(CAST(display AS BLOB)) BETWEEN 1 AND 128),
    name_key TEXT NOT NULL CHECK (length(CAST(name_key AS BLOB)) BETWEEN 1 AND 128),
    PRIMARY KEY (person_id, ordinal),
    UNIQUE (person_id, name_key)
) STRICT;
CREATE INDEX person_names_key ON person_names(name_key, person_id);

CREATE TABLE voice_signatures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL CHECK (length(CAST(model_id AS BLOB)) BETWEEN 1 AND 128),
    model_version TEXT NOT NULL CHECK (length(CAST(model_version AS BLOB)) BETWEEN 1 AND 128),
    role TEXT NOT NULL CHECK (role IN ('anchor', 'enrolment', 'refinement')),
    dimension INTEGER NOT NULL CHECK (dimension BETWEEN 1 AND 4096),
    speech_ms INTEGER NOT NULL CHECK (speech_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    source_session_id INTEGER NOT NULL CHECK (source_session_id >= 0),
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL CHECK (length(ciphertext) = dimension * 4 + 16)
) STRICT;
CREATE INDEX voice_signatures_model ON voice_signatures(model_id, model_version, person_id, id);

CREATE TABLE signature_key_check (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL
) STRICT;

-- memories: the 0001 definition verbatim, plus the subject column.
CREATE TABLE memories_next (
    -- every column and CHECK from 0001_controlled_memory.sql, unchanged, then:
    subject_person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE
) STRICT;
INSERT INTO memories_next SELECT *, 1 FROM memories;
-- INSERT sets the counter to the highest surviving id; carry the old high-water
-- mark so ids of deleted memories are never reused.
UPDATE sqlite_sequence
    SET seq = max(seq, coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'memories'), 0))
    WHERE name = 'memories_next';
DROP TABLE memories;
ALTER TABLE memories_next RENAME TO memories;
CREATE INDEX memories_retrieval_order ON memories(id, state, updated_at_ms);
CREATE INDEX memories_subject ON memories(subject_person_id, id);
```

Copy the 0001 column list and CHECKs into `memories_next` literally; the
`SELECT *, 1` relies on the same column order.

- [ ] **Step 5: Implement the upgrade in `crates/memory/src/sqlite.rs`**

Set `SCHEMA_VERSION = 2`; keep `MIGRATION` as `MIGRATION_1` and add
`MIGRATION_2 = include_str!("../migrations/0002_people.sql")`, each with its own
FNV checksum (`migration_checksum(sql: &str)`). Add
`fn upgrade_to_version_2(connection: &mut Connection) -> MemoryStoreResult<()>`
with this order, because SQLite ignores `PRAGMA foreign_keys` inside a
transaction and a table rebuild must run with it off:

1. `PRAGMA foreign_keys = OFF` (outside any transaction).
2. `BEGIN IMMEDIATE`; `execute_batch(MIGRATION_2)`;
   `INSERT INTO schema_migrations (version, checksum) VALUES (2, ?)`.
3. `SELECT count(*) FROM pragma_foreign_key_check` must be 0, else roll back
   and return `InvalidDatabase`.
4. `PRAGMA user_version = 2` inside the transaction; `COMMIT`.
5. `PRAGMA foreign_keys = ON`, then `validate_database`.

`open()`: after `open_connection`, read `user_version`. If it is 1, validate
version 1 (application id, checksum row 1 equals `MIGRATION_1`'s checksum,
no foreign-key failures) and call `upgrade_to_version_2`; any failure returns
before step 4 commits, so the file stays version 1. `initialize_database` runs
`MIGRATION_1`, inserts row 1, sets `user_version = 1`, then calls
`upgrade_to_version_2`, so fresh and upgraded databases take one path.
`validate_database` (run on every `connection()`) accepts only version 2 with
both checksum rows matching. `create` writes `subject_person_id =
PersonId::OWNER` until Task 2 replaces it with the draft's subject.

- [ ] **Step 6: Run the crate's tests**

Run: `cargo test -p conversation-memory`
Expected: PASS, including the updated `sqlite_lifecycle` cases.

- [ ] **Step 7: Commit**

```bash
git add crates/memory crates/protocol/src/memory.rs crates/protocol/src/lib.rs
git commit -m "feat(memory): schema version 2 with people and an in-place upgrade"
```

---

### Task 2: Memory subject and subject-scoped retrieval

**Files:**
- Modify: `crates/protocol/src/memory.rs` (`MemoryDraft`, `MemoryRetrievalRequest`)
- Modify: `crates/memory/src/sqlite.rs` (`create`, `RECORD_QUERY`, `row_to_record`, `retrieve`)
- Test: `crates/memory/tests/subject.rs`, `crates/protocol/tests/memory_contracts.rs`

**Interfaces:**
- Consumes: `PersonId`, `PersonId::OWNER` (Task 1).
- Produces: `MemoryDraft::with_subject(self, PersonId) -> Self`,
  `MemoryDraft::subject(&self) -> PersonId` (default `PersonId::OWNER` from
  `MemoryDraft::new`, whose signature does not change);
  `MemoryRecord::subject(&self) -> PersonId`;
  `MemoryRetrievalRequest::with_subject(self, PersonId) -> Self`,
  `MemoryRetrievalRequest::subject(&self) -> Option<PersonId>` (`None` means
  every subject, today's behaviour).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/protocol/tests/memory_contracts.rs
#[test]
fn drafts_default_to_the_owner_subject() {
    assert_eq!(draft("x").subject(), PersonId::OWNER);
    assert_eq!(draft("x").with_subject(person(7)).subject(), person(7));
}

// crates/memory/tests/subject.rs  (people created with raw SQL until Task 3:
// INSERT INTO people (is_owner, maturity, created_at_ms) VALUES (0, 'mature', 0))
#[test]
fn upgraded_memories_belong_to_the_owner() {
    // open a copy of tests/fixtures/memory-v1.sqlite3 (see schema_upgrade.rs)
    assert!(store.list(far_future()).unwrap().iter().all(|r| r.subject() == PersonId::OWNER));
}

#[test]
fn created_memory_keeps_its_subject() {
    let jack = insert_person(&database);
    let record = store.create(draft("Jack likes tea").with_subject(jack)).unwrap();
    assert_eq!(record.subject(), jack);
    assert_eq!(store.inspect(record.id(), now()).unwrap().subject(), jack);
}

#[test]
fn memory_for_an_unknown_person_is_refused() {
    let error = store.create(draft("x").with_subject(person(99))).unwrap_err();
    assert_eq!(error.kind(), MemoryStoreErrorKind::NotFound);
}

#[test]
fn unscoped_retrieval_returns_every_subject() {
    // owner "tea at noon", Jack "tea with lemon"; query "tea" without with_subject → both ids
}

#[test]
fn subject_scope_returns_only_that_person() {
    // same data; with_subject(jack) → only Jack's id; with_subject(OWNER) → only the owner's
}

#[test]
fn subject_scope_excludes_other_peoples_pinned_matches() {
    // owner memory "tea" pinned; Jack has none matching; with_subject(jack), query "tea" → no items
}

#[test]
fn editing_a_memory_keeps_its_subject() {
    // create with Jack, edit content via MemoryPatch → record.subject() == jack
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-protocol --test memory_contracts drafts_default && cargo test -p conversation-memory --test subject`
Expected: FAIL to compile (`with_subject` missing).

- [ ] **Step 3: Implement**

Add a `subject: PersonId` field to `MemoryDraft` (set to `PersonId::OWNER` in
`new`) and a `subject: Option<PersonId>` field to `MemoryRetrievalRequest`.
`create` checks `SELECT 1 FROM people WHERE id = ?` inside its transaction and
returns `not_found_error()` when absent, then inserts the draft's subject.
Append `m.subject_person_id` as the last column of `RECORD_QUERY` (index 24)
and apply it with `with_subject` in `row_to_record`. In `retrieve`, add
`WHERE m.subject_person_id = ?` before `ORDER BY` when `subject()` is `Some`,
so the 1,024-record scan bound counts only that person's records. Subject
changes do not bump `revision`: the subject is not client-visible.

- [ ] **Step 4: Run the workspace tests**

Run: `cargo test --workspace --locked`
Expected: PASS. No other call site changes, since `MemoryDraft::new` keeps its signature.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol crates/memory
git commit -m "feat(memory): every memory has a subject person, retrieval can be scoped to one"
```

---

### Task 3: People records

**Files:**
- Create: `crates/memory/src/people.rs`
- Modify: `crates/memory/src/lib.rs` (module, exports), `crates/memory/src/sqlite.rs` (`impl PeopleStore for SqliteMemoryStore`)
- Test: `crates/memory/tests/people.rs`

**Interfaces:**
- Consumes: `PersonId` (Task 1), `MemoryDraft::with_subject` (Task 2, in tests).
- Produces, in `conversation_memory`:
  - `pub const MAX_PEOPLE: usize = 256;`
  - `PersonName::new(display: impl Into<String>, key: impl Into<String>) -> MemoryStoreResult<PersonName>`;
    `display(&self) -> &str`, `key(&self) -> &str`. Each 1..=128 bytes and not
    blank after trimming, else `Conflict`. The key is the caller's normalised
    form (Plan 2 computes pinyin or case-folded keys); the store compares keys
    exactly.
  - `enum PersonMaturity { Provisional, Mature }`
  - `Person`: `id() -> PersonId`, `is_owner() -> bool`, `names() -> &[PersonName]`
    (primary first; empty only for an owner whose name is not set),
    `display_name() -> Option<&str>`, `maturity() -> PersonMaturity`,
    `needs_review() -> bool`, `created_at() -> UnixTimestampMillis`.
  - `NewPerson::new(name: PersonName, needs_review: bool, created_at: UnixTimestampMillis) -> NewPerson`
  - `trait PeopleStore: Send + Sync` with:
    - `people(&self) -> MemoryStoreResult<Vec<Person>>` (ordered by id)
    - `person(&self, id: PersonId) -> MemoryStoreResult<Person>`
    - `people_named(&self, key: &str) -> MemoryStoreResult<Vec<PersonId>>` (any ordinal, ordered by id)
    - `create_person(&self, person: NewPerson) -> MemoryStoreResult<Person>` (never the owner; starts `Provisional`)
    - `add_name(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person>` (alias, appended; `Conflict` if that person already has the key)
    - `rename(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person>` (replaces ordinal 0, inserting it for an unnamed owner; drops an alias with the same key; clears `needs_review`)
    - `mark_mature(&self, id: PersonId) -> MemoryStoreResult<Person>`
    - `delete_person(&self, id: PersonId) -> MemoryStoreResult<()>` (`Conflict` for the owner)
    - `merge_people(&self, from: PersonId, into: PersonId) -> MemoryStoreResult<Person>` (`Conflict` if `from` is the owner or `from == into`)
  - Unknown ids return `NotFound` everywhere.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn created_person_is_provisional_and_findable_by_key() {
    let jack = store.create_person(new_person("Jack", "jack", false)).unwrap();
    assert_eq!(jack.maturity(), PersonMaturity::Provisional);
    assert!(!jack.is_owner());
    assert_eq!(store.people_named("jack").unwrap(), vec![jack.id()]);
}

#[test]
fn two_people_may_share_a_name() {
    // two create_person("Jack","jack"), second with needs_review = true → people_named returns both ids
}

#[test]
fn alias_is_found_and_duplicate_alias_is_refused() {
    // add_name(chris, "昊辰", "haochen") → people_named("haochen") == [chris]; again → Conflict
}

#[test]
fn owner_starts_unnamed_and_rename_sets_the_name() {
    let owner = store.person(PersonId::OWNER).unwrap();
    assert!(owner.is_owner() && owner.display_name().is_none());
    let owner = store.rename(PersonId::OWNER, name("Chris", "chris")).unwrap();
    assert_eq!(owner.display_name(), Some("Chris"));
}

#[test]
fn rename_clears_needs_review_and_absorbs_a_matching_alias() {}

#[test]
fn deleting_a_person_deletes_only_their_memories() {
    // Jack memory and owner memory; delete_person(jack) → store.list() has only the owner's;
    // inspect(jack_memory) → NotFound; people_named("jack") → []
}

#[test]
fn owner_cannot_be_deleted_or_merged_away() {
    assert_eq!(store.delete_person(PersonId::OWNER).unwrap_err().kind(), MemoryStoreErrorKind::Conflict);
    assert_eq!(store.merge_people(PersonId::OWNER, jack).unwrap_err().kind(), MemoryStoreErrorKind::Conflict);
}

#[test]
fn merge_moves_memories_and_names_and_removes_the_source() {
    // jack2 (needs_review, alias "jake") merged into jack: jack2's memory now has subject jack,
    // jack.names() == ["Jack", "Jake"], person(jack2) → NotFound, merged.needs_review() == false,
    // merged maturity is Mature if either was
}

#[test]
fn merging_a_duplicate_into_the_owner_is_allowed() {}

#[test]
fn person_count_is_bounded() {
    // 255 created (owner is the 256th) → next create_person → LimitExceeded
}

#[test]
fn blank_or_oversized_names_are_refused() {
    // PersonName::new("  ", "x"), ("x", ""), 129-byte display → Conflict
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-memory --test people`
Expected: FAIL to compile (`PeopleStore` missing).

- [ ] **Step 3: Implement `PeopleStore for SqliteMemoryStore`**

Each method is one `IMMEDIATE` transaction. `merge_people` order: move
memories (`UPDATE memories SET subject_person_id`), move signatures
(`UPDATE voice_signatures SET person_id`), append `from`'s names after
`into`'s highest ordinal skipping keys `into` already has, set `into`'s
maturity to mature if either was and `needs_review = 0`, then delete `from`.
`delete_person` is `DELETE FROM people WHERE id = ? AND is_owner = 0`; the
cascades remove names, signatures, memories and their sources and retrieval
items.

- [ ] **Step 4: Run the crate's tests**

Run: `cargo test -p conversation-memory`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/memory
git commit -m "feat(memory): people with names, aliases, an owner, deletion and merge"
```

---

### Task 4: Encrypted voice signatures

**Files:**
- Create: `crates/memory/src/signatures.rs`
- Modify: `crates/memory/Cargo.toml` (`ring.workspace = true`), `crates/memory/src/error.rs` (two kinds), `crates/memory/src/lib.rs`, `crates/memory/src/sqlite.rs`
- Test: `crates/memory/tests/signatures.rs`

**Interfaces:**
- Consumes: `PeopleStore`, `Person` (Task 3); `SessionId` from `conversation_protocol`.
- Produces, in `conversation_memory`:
  - `MemoryStoreErrorKind::KeyUnavailable` (variable unset or not 64 lowercase hex) and `MemoryStoreErrorKind::KeyMismatch` (key differs from the one the store was first used with).
  - `pub const MAX_SIGNATURES_PER_PERSON: usize = 64;`
  - `SignatureKey::from_hex(material: &str) -> MemoryStoreResult<SignatureKey>`, `SignatureKey::from_environment(name: &str) -> MemoryStoreResult<SignatureKey>` (reads the variable, then `from_hex`). `Debug` prints `SignatureKey(..)`.
  - `SignatureModel::new(id: impl Into<String>, version: impl Into<String>) -> MemoryStoreResult<SignatureModel>` (each 1..=128 bytes).
  - `enum SignatureRole { Anchor, Enrolment, Refinement }`
  - `VoiceSignatureDraft::new(model: SignatureModel, role: SignatureRole, embedding: Vec<f32>, speech_ms: u32, created_at: UnixTimestampMillis, source_session: SessionId) -> MemoryStoreResult<VoiceSignatureDraft>` (dimension 1..=4096, every value finite, else `Conflict`).
  - `SignatureId` (u64 newtype with `get()`), `VoiceSignature`: `id()`, `person()`, `model()`, `role()`, `embedding() -> &[f32]`, `speech_ms()`, `created_at()`, `source_session()`.
  - On `PeopleStore`:
    - `verify_signature_key(&self, key: &SignatureKey) -> MemoryStoreResult<()>` (first use stores the check row; later uses must decrypt it)
    - `add_signature(&self, key: &SignatureKey, person: PersonId, draft: VoiceSignatureDraft) -> MemoryStoreResult<VoiceSignature>`
    - `signatures(&self, key: &SignatureKey, model: &SignatureModel) -> MemoryStoreResult<Vec<VoiceSignature>>` (every person's, that model and version only, ordered by person then id)
    - `remove_signature(&self, id: SignatureId) -> MemoryStoreResult<()>` (`Conflict` unless the role is `Refinement`: anchor and enrolment samples are immutable)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn signature_round_trips_and_is_not_stored_in_clear() {
    let stored = store.add_signature(&key_a(), jack, draft(&[0.25, -0.5, 1.0])).unwrap();
    assert_eq!(store.signatures(&key_a(), &ecapa()).unwrap(), vec![stored.clone()]);
    let blob: Vec<u8> = raw(&database).query_row("SELECT ciphertext FROM voice_signatures", [], |r| r.get(0)).unwrap();
    assert!(!contains_subslice(&blob, &0.25_f32.to_le_bytes()));
}

#[test]
fn wrong_key_is_refused_rather_than_reading_empty() {
    store.add_signature(&key_a(), jack, draft(&[1.0])).unwrap();
    assert_eq!(store.verify_signature_key(&key_b()).unwrap_err().kind(), MemoryStoreErrorKind::KeyMismatch);
    assert_eq!(store.signatures(&key_b(), &ecapa()).unwrap_err().kind(), MemoryStoreErrorKind::KeyMismatch);
    assert_eq!(store.add_signature(&key_b(), jack, draft(&[1.0])).unwrap_err().kind(), MemoryStoreErrorKind::KeyMismatch);
}

#[test]
fn malformed_key_material_is_unavailable() {
    for material in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
        assert_eq!(SignatureKey::from_hex(material).unwrap_err().kind(), MemoryStoreErrorKind::KeyUnavailable);
    }
}

#[test]
fn other_models_signatures_are_never_returned() {
    // add under ("ecapa","1") and ("eres2net","1") and ("ecapa","2") → signatures(ecapa v1) has exactly one
}

#[test]
fn tampered_ciphertext_is_an_invalid_database() {
    // flip one byte of ciphertext with raw SQL → signatures(&key_a()) → InvalidDatabase
}

#[test]
fn only_refinement_samples_can_be_removed() {
    // anchor and enrolment → remove_signature → Conflict; refinement → Ok and gone
}

#[test]
fn non_finite_or_empty_embeddings_are_refused() {
    // [] , [f32::NAN], 4097 values → VoiceSignatureDraft::new → Conflict
}

#[test]
fn signatures_per_person_are_bounded() {
    // 64 added → 65th → LimitExceeded
}

#[test]
fn deleting_a_person_deletes_their_signatures() {}

#[test]
fn merging_moves_signatures_to_the_surviving_person() {}

#[test]
fn key_debug_output_hides_the_key() {
    assert_eq!(format!("{:?}", key_a()), "SignatureKey(..)");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-memory --test signatures`
Expected: FAIL to compile (`SignatureKey` missing).

- [ ] **Step 3: Implement**

Use `ring::aead` AES-256-GCM (`LessSafeKey`, `UnboundKey::new(&AES_256_GCM, …)`)
and a fresh 12-byte nonce from `ring::rand::SystemRandom` per row, as
`crates/runtime/src/session_recording.rs` does. The plaintext is the embedding
as little-endian `f32`. The AAD is
`b"conversation-runtime/voice-signature/v1\0"` followed by model id, `\0`,
model version, `\0`, role as stored, `\0`, dimension as little-endian `u32`, so
a row whose metadata was edited fails to decrypt. The key check row encrypts
the fixed plaintext `b"conversation-runtime/voice-signature-key/v1"` with AAD
`b"key-check"`. `add_signature` and `signatures` run the key check inside
their own transaction before touching a signature row; a row that fails to
decrypt after the check passed is `InvalidDatabase`.

- [ ] **Step 4: Run all gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/memory Cargo.lock
git commit -m "feat(memory): encrypted voice signatures with their own key"
```
