-- Schema version 2: people, voice signatures, and a subject person on every memory.
-- Runs with foreign keys off (a table rebuild); the caller checks them before commit.

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

CREATE TABLE memories_next (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('working', 'episodic', 'semantic', 'identity', 'relationship')),
    state TEXT NOT NULL CHECK (state IN ('candidate', 'active', 'expired')),
    content TEXT NOT NULL CHECK (length(CAST(content AS BLOB)) BETWEEN 1 AND 4096),
    confidence INTEGER NOT NULL CHECK (confidence BETWEEN 0 AND 1000),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    retention_kind TEXT NOT NULL CHECK (retention_kind IN ('working', 'session', 'until', 'until_deleted')),
    expires_at_ms INTEGER CHECK (expires_at_ms >= 0),
    session_id INTEGER CHECK (session_id >= 0),
    pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    prior_retention_kind TEXT CHECK (prior_retention_kind IN ('session', 'until', 'until_deleted')),
    prior_expires_at_ms INTEGER CHECK (prior_expires_at_ms >= 0),
    prior_session_id INTEGER CHECK (prior_session_id >= 0),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    last_used_at_ms INTEGER CHECK (last_used_at_ms >= created_at_ms),
    last_retrieval_reason TEXT CHECK (last_retrieval_reason IN ('pinned_match', 'exact_phrase', 'shared_term', 'recent_working')),
    subject_person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    CHECK (
        (retention_kind IN ('working', 'until') AND expires_at_ms IS NOT NULL AND session_id IS NULL)
        OR (retention_kind = 'session' AND expires_at_ms IS NULL AND session_id IS NOT NULL)
        OR (retention_kind = 'until_deleted' AND expires_at_ms IS NULL AND session_id IS NULL)
    ),
    CHECK (kind != 'working' OR retention_kind = 'working'),
    CHECK (kind = 'working' OR retention_kind != 'working'),
    CHECK (kind != 'working' OR pinned = 0),
    CHECK (
        (pinned = 0 AND prior_retention_kind IS NULL AND prior_expires_at_ms IS NULL AND prior_session_id IS NULL)
        OR (
            pinned = 1 AND retention_kind = 'until_deleted' AND (
                (prior_retention_kind = 'until' AND prior_expires_at_ms IS NOT NULL AND prior_session_id IS NULL)
                OR (prior_retention_kind = 'session' AND prior_expires_at_ms IS NULL AND prior_session_id IS NOT NULL)
                OR (prior_retention_kind = 'until_deleted' AND prior_expires_at_ms IS NULL AND prior_session_id IS NULL)
            )
        )
    )
) STRICT;
INSERT INTO memories_next SELECT *, 1 FROM memories;
-- INSERT sets the counter to the highest surviving id; carry the old high-water mark
-- so the ids of deleted memories are never handed out again.
UPDATE sqlite_sequence
    SET seq = max(seq, coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'memories'), 0))
    WHERE name = 'memories_next';
DROP TABLE memories;
ALTER TABLE memories_next RENAME TO memories;
CREATE INDEX memories_retrieval_order ON memories(id, state, updated_at_ms);
CREATE INDEX memories_subject ON memories(subject_person_id, id);
