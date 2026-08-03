CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version = 1),
    checksum TEXT NOT NULL CHECK (length(checksum) = 16)
) STRICT;

CREATE TABLE memories (
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

CREATE INDEX memories_retrieval_order ON memories(id, state, updated_at_ms);

CREATE TABLE memory_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('user_provided', 'user_edited', 'completed_exchange', 'application_imported', 'user_approved')),
    source_id TEXT NOT NULL CHECK (length(CAST(source_id AS BLOB)) BETWEEN 1 AND 512),
    source_timestamp_ms INTEGER NOT NULL CHECK (source_timestamp_ms >= 0),
    actor TEXT NOT NULL CHECK (length(CAST(actor AS BLOB)) BETWEEN 1 AND 256),
    content_digest TEXT CHECK (length(CAST(content_digest AS BLOB)) BETWEEN 1 AND 256),
    confirmation_id TEXT CHECK (length(CAST(confirmation_id AS BLOB)) BETWEEN 1 AND 512),
    approved_revision INTEGER CHECK (approved_revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    CHECK (
        (kind = 'user_approved' AND confirmation_id = source_id AND approved_revision IS NOT NULL AND content_digest IS NOT NULL)
        OR (kind != 'user_approved' AND confirmation_id IS NULL AND approved_revision IS NULL)
    )
) STRICT;

CREATE INDEX memory_sources_memory_id ON memory_sources(memory_id, id);

CREATE TABLE retrieval_traces (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id INTEGER NOT NULL CHECK (turn_id >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    maximum_items INTEGER NOT NULL CHECK (maximum_items BETWEEN 1 AND 8),
    maximum_bytes INTEGER NOT NULL CHECK (maximum_bytes BETWEEN 1 AND 8192),
    selected_items INTEGER NOT NULL CHECK (selected_items BETWEEN 0 AND maximum_items),
    used_bytes INTEGER NOT NULL CHECK (used_bytes BETWEEN 0 AND maximum_bytes),
    excluded_by_state INTEGER NOT NULL CHECK (excluded_by_state >= 0),
    excluded_by_expiry INTEGER NOT NULL CHECK (excluded_by_expiry >= 0),
    excluded_by_relevance INTEGER NOT NULL CHECK (excluded_by_relevance >= 0),
    excluded_by_item_limit INTEGER NOT NULL CHECK (excluded_by_item_limit >= 0),
    excluded_by_byte_limit INTEGER NOT NULL CHECK (excluded_by_byte_limit >= 0)
) STRICT;

CREATE TABLE retrieval_items (
    trace_id INTEGER NOT NULL REFERENCES retrieval_traces(id) ON DELETE CASCADE,
    memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    reason TEXT NOT NULL CHECK (reason IN ('pinned_match', 'exact_phrase', 'shared_term', 'recent_working')),
    content_bytes INTEGER NOT NULL CHECK (content_bytes BETWEEN 1 AND 4096),
    PRIMARY KEY (trace_id, ordinal),
    UNIQUE (trace_id, memory_id)
) STRICT;
