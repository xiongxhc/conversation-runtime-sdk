use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use conversation_protocol::{
    MemoryApproval, MemoryApprovalEvidence, MemoryConfidence, MemoryContextItem, MemoryDraft,
    MemoryId, MemoryInspection, MemoryKind, MemoryPatch, MemoryProvenance, MemoryProvenanceKind,
    MemoryRecord, MemoryRetention, MemoryRetrievalReason, MemoryRetrievalRequest,
    MemoryRetrievalTrace, MemoryState, MemoryTraceItem, RetrievalTraceId, SessionId,
    UnixTimestampMillis, MAX_MEMORY_INSPECTION_HISTORY_ITEMS, MAX_MEMORY_LIST_PAGE_ITEMS,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use crate::retrieval::{check_cancellation, select_records};
use crate::{
    BoundedMemoryInspection, MemoryPage, MemoryRetrieval, MemoryStore, MemoryStoreError,
    MemoryStoreErrorKind, MemoryStoreResult, RetrievalCancellation, MAX_MEMORY_SCAN_RECORDS,
};

pub const SQLITE_APPLICATION_ID: u32 = 0x4352_544d;
pub const SCHEMA_VERSION: u32 = 1;

const MIGRATION: &str = include_str!("../migrations/0001_controlled_memory.sql");
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub struct SqliteMemoryStore {
    database_path: PathBuf,
}

impl SqliteMemoryStore {
    pub fn initialize(database_path: impl AsRef<Path>) -> MemoryStoreResult<Self> {
        let database_path = validate_path(database_path.as_ref())?;
        if database_path.exists() {
            return Self::open(database_path);
        }

        let parent = database_path.parent().ok_or_else(|| {
            store_error(
                MemoryStoreErrorKind::InvalidPath,
                "memory database path has no parent",
            )
        })?;
        if !parent.exists() {
            fs::create_dir(parent).map_err(|_| storage_error())?;
            set_directory_permissions(parent)?;
        }

        create_database_file(&database_path)?;
        let result = initialize_database(&database_path);
        if result.is_err() {
            let _ = fs::remove_file(&database_path);
        }
        result?;
        Self::open(database_path)
    }

    pub fn open(database_path: impl AsRef<Path>) -> MemoryStoreResult<Self> {
        let database_path = validate_path(database_path.as_ref())?;
        if !database_path.exists() {
            return Err(store_error(
                MemoryStoreErrorKind::NotInitialized,
                "memory database is not initialized",
            ));
        }
        if !fs::metadata(&database_path)
            .map_err(|_| storage_error())?
            .is_file()
        {
            return Err(store_error(
                MemoryStoreErrorKind::InvalidPath,
                "memory database path is not a regular file",
            ));
        }

        let connection = open_connection(&database_path)?;
        validate_database(&connection)?;
        drop(connection);
        validate_file_permissions(&database_path)?;
        Ok(Self { database_path })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    fn connection(&self) -> MemoryStoreResult<Connection> {
        let connection = open_connection(&self.database_path)?;
        validate_database(&connection)?;
        Ok(connection)
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn create(&self, draft: MemoryDraft) -> MemoryStoreResult<MemoryRecord> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let (retention_kind, expires_at, session_id) = retention_columns(draft.retention())?;
        transaction
            .execute(
                concat!(
                    "INSERT INTO memories (kind, state, content, confidence, created_at_ms, ",
                    "updated_at_ms, retention_kind, expires_at_ms, session_id) ",
                    "VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)"
                ),
                params![
                    draft.kind().as_str(),
                    draft.initial_state().as_str(),
                    draft.content(),
                    draft.confidence().get(),
                    draft.created_at().get(),
                    retention_kind,
                    expires_at,
                    session_id,
                ],
            )
            .map_err(map_sqlite_error)?;
        let memory_id = transaction.last_insert_rowid();
        insert_source(
            &transaction,
            memory_id,
            draft.provenance(),
            draft.created_at(),
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.inspect(checked_memory_id(memory_id)?, draft.created_at())
    }

    fn list(&self, now: UnixTimestampMillis) -> MemoryStoreResult<Vec<MemoryRecord>> {
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)?;
        let mut statement = connection
            .prepare(&format!("{RECORD_QUERY} ORDER BY m.id"))
            .map_err(map_sqlite_error)?;
        let records = statement
            .query_map([], row_to_record)
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(records)
    }

    fn list_page(
        &self,
        now: UnixTimestampMillis,
        before_id: Option<MemoryId>,
        limit: usize,
    ) -> MemoryStoreResult<MemoryPage> {
        validate_limit(
            limit,
            MAX_MEMORY_LIST_PAGE_ITEMS,
            "memory list page limit must be 1 through 50",
        )?;
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)?;
        let before_id = before_id
            .map(|memory_id| sqlite_integer(memory_id.get()))
            .transpose()?;
        let mut statement = connection
            .prepare(&format!(
                "{RECORD_QUERY} WHERE (?1 IS NULL OR m.id < ?1) ORDER BY m.id DESC LIMIT ?2"
            ))
            .map_err(map_sqlite_error)?;
        let mut records = statement
            .query_map(
                params![before_id, sqlite_integer((limit + 1) as u64)?],
                row_to_record,
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        let next_before_id = (records.len() > limit)
            .then(|| records.pop())
            .flatten()
            .and_then(|_| records.last().map(MemoryRecord::id));
        Ok(MemoryPage::new(records, next_before_id))
    }

    fn inspect(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryRecord> {
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)?;
        connection
            .query_row(
                &format!("{RECORD_QUERY} WHERE m.id = ?1"),
                [sqlite_integer(memory_id.get())?],
                row_to_record,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(not_found_error)
    }

    fn inspect_with_sources(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryInspection> {
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(map_sqlite_error)?;
        let record = transaction
            .query_row(
                &format!("{RECORD_QUERY} WHERE m.id = ?1"),
                [sqlite_integer(memory_id.get())?],
                row_to_record,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(not_found_error)?;
        let sources = {
            let mut statement = transaction
                .prepare(concat!(
                    "SELECT kind, source_id, source_timestamp_ms, actor, content_digest ",
                    "FROM memory_sources WHERE memory_id = ?1 AND kind != 'user_approved' ORDER BY id"
                ))
                .map_err(map_sqlite_error)?;
            let sources = statement
                .query_map([sqlite_integer(memory_id.get())?], row_to_provenance)
                .map_err(map_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_error)?;
            sources
        };
        let approvals = {
            let mut statement = transaction
                .prepare(concat!(
                    "SELECT confirmation_id, actor, source_timestamp_ms, approved_revision, content_digest ",
                    "FROM memory_sources WHERE memory_id = ?1 AND kind = 'user_approved' ORDER BY id"
                ))
                .map_err(map_sqlite_error)?;
            let approvals = statement
                .query_map([sqlite_integer(memory_id.get())?], row_to_approval)
                .map_err(map_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_error)?;
            approvals
        };
        let inspection = MemoryInspection::new(record, sources, approvals)
            .map_err(|_| invalid_database_error())?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(inspection)
    }

    fn inspect_bounded(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
        history_limit: usize,
    ) -> MemoryStoreResult<BoundedMemoryInspection> {
        validate_limit(
            history_limit,
            MAX_MEMORY_INSPECTION_HISTORY_ITEMS,
            "memory inspection history limit must be 1 through 32",
        )?;
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(map_sqlite_error)?;
        let memory_id = sqlite_integer(memory_id.get())?;
        let record = transaction
            .query_row(
                &format!("{RECORD_QUERY} WHERE m.id = ?1"),
                [memory_id],
                row_to_record,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(not_found_error)?;
        let (sources, sources_truncated) = bounded_history(
            &transaction,
            memory_id,
            history_limit,
            concat!(
                "SELECT kind, source_id, source_timestamp_ms, actor, content_digest ",
                "FROM memory_sources WHERE memory_id = ?1 AND kind != 'user_approved' ",
                "ORDER BY id DESC LIMIT ?2"
            ),
            row_to_provenance,
        )?;
        let (approvals, approvals_truncated) = bounded_history(
            &transaction,
            memory_id,
            history_limit,
            concat!(
                "SELECT confirmation_id, actor, source_timestamp_ms, approved_revision, content_digest ",
                "FROM memory_sources WHERE memory_id = ?1 AND kind = 'user_approved' ",
                "ORDER BY id DESC LIMIT ?2"
            ),
            row_to_approval,
        )?;
        let inspection = MemoryInspection::new(record, sources, approvals)
            .map_err(|_| invalid_database_error())?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(BoundedMemoryInspection::new(
            inspection,
            sources_truncated,
            approvals_truncated,
        ))
    }

    fn edit(&self, memory_id: MemoryId, patch: MemoryPatch) -> MemoryStoreResult<MemoryRecord> {
        let current = self.inspect(memory_id, patch.edited_at())?;
        if current.revision() != patch.expected_revision() {
            return Err(conflict_error());
        }
        if patch.edited_at() < current.updated_at() {
            return Err(conflict_error());
        }
        if current.pinned() && patch.retention().is_some() {
            return Err(conflict_error());
        }

        let content = patch.content().unwrap_or(current.content());
        let content_changed = content != current.content();
        let confidence = patch.confidence().unwrap_or(current.confidence());
        let retention = patch.retention().unwrap_or(current.retention()).clone();
        MemoryDraft::new(
            current.kind(),
            content,
            patch.provenance().clone(),
            confidence,
            current.created_at(),
            retention.clone(),
        )
        .map_err(|_| {
            store_error(
                MemoryStoreErrorKind::Conflict,
                "memory edit violates record retention",
            )
        })?;

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let (retention_kind, expires_at, session_id) = retention_columns(&retention)?;
        let state = if current.kind().requires_approval()
            && current.state() == MemoryState::Active
            && content_changed
        {
            MemoryState::Candidate
        } else {
            current.state()
        };
        let changed = transaction
            .execute(
                concat!(
                    "UPDATE memories SET content = ?1, confidence = ?2, updated_at_ms = ?3, ",
                    "retention_kind = ?4, expires_at_ms = ?5, session_id = ?6, state = ?7, ",
                    "revision = revision + 1 WHERE id = ?8 AND revision = ?9"
                ),
                params![
                    content,
                    confidence.get(),
                    patch.edited_at().get(),
                    retention_kind,
                    expires_at,
                    session_id,
                    state.as_str(),
                    sqlite_integer(memory_id.get())?,
                    sqlite_integer(patch.expected_revision())?,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(conflict_error());
        }
        insert_source(
            &transaction,
            i64::try_from(memory_id.get()).map_err(|_| invalid_database_error())?,
            patch.provenance(),
            patch.edited_at(),
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.inspect(memory_id, patch.edited_at())
    }

    fn approve(
        &self,
        memory_id: MemoryId,
        approval: MemoryApproval,
    ) -> MemoryStoreResult<MemoryRecord> {
        let current = self.inspect(memory_id, approval.confirmed_at())?;
        if current.revision() != approval.expected_revision()
            || current.state() != MemoryState::Candidate
            || !current.kind().requires_approval()
            || approval.confirmed_at() < current.updated_at()
        {
            return Err(conflict_error());
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let changed = transaction
            .execute(
                concat!(
                    "UPDATE memories SET state = 'active', updated_at_ms = ?1, ",
                    "revision = revision + 1 WHERE id = ?2 AND revision = ?3 ",
                    "AND state = 'candidate' AND kind IN ('identity', 'relationship')"
                ),
                params![
                    approval.confirmed_at().get(),
                    sqlite_integer(memory_id.get())?,
                    sqlite_integer(approval.expected_revision())?,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(conflict_error());
        }
        let evidence = approval.evidence_for(current.content());
        insert_approval(
            &transaction,
            sqlite_integer(memory_id.get())?,
            &evidence,
            approval.confirmed_at(),
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.inspect(memory_id, approval.confirmed_at())
    }

    fn set_pinned(
        &self,
        memory_id: MemoryId,
        expected_revision: u64,
        pinned: bool,
        changed_at: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryRecord> {
        let current = self.inspect(memory_id, changed_at)?;
        if current.revision() != expected_revision
            || current.state() == MemoryState::Expired
            || current.kind() == MemoryKind::Working
            || current.pinned() == pinned
            || changed_at < current.updated_at()
        {
            return Err(conflict_error());
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let changed = if pinned {
            transaction.execute(
                concat!(
                    "UPDATE memories SET pinned = 1, prior_retention_kind = retention_kind, ",
                    "prior_expires_at_ms = expires_at_ms, prior_session_id = session_id, ",
                    "retention_kind = 'until_deleted', expires_at_ms = NULL, session_id = NULL, ",
                    "updated_at_ms = ?1, revision = revision + 1 ",
                    "WHERE id = ?2 AND revision = ?3 AND pinned = 0 AND kind != 'working'"
                ),
                params![
                    changed_at.get(),
                    sqlite_integer(memory_id.get())?,
                    sqlite_integer(expected_revision)?,
                ],
            )
        } else {
            transaction.execute(
                concat!(
                    "UPDATE memories SET pinned = 0, retention_kind = prior_retention_kind, ",
                    "expires_at_ms = prior_expires_at_ms, session_id = prior_session_id, ",
                    "prior_retention_kind = NULL, prior_expires_at_ms = NULL, ",
                    "prior_session_id = NULL, updated_at_ms = ?1, revision = revision + 1 ",
                    "WHERE id = ?2 AND revision = ?3 AND pinned = 1 ",
                    "AND prior_retention_kind IS NOT NULL"
                ),
                params![
                    changed_at.get(),
                    sqlite_integer(memory_id.get())?,
                    sqlite_integer(expected_revision)?,
                ],
            )
        }
        .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(conflict_error());
        }
        transaction.commit().map_err(map_sqlite_error)?;
        self.inspect(memory_id, changed_at)
    }

    fn prune_expired(&self, now: UnixTimestampMillis) -> MemoryStoreResult<usize> {
        let mut connection = self.connection()?;
        expire_due(&mut connection, now)
    }

    fn expire_session(
        &self,
        session_id: SessionId,
        expired_at: UnixTimestampMillis,
    ) -> MemoryStoreResult<usize> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let changed = transaction
            .execute(
                concat!(
                    "UPDATE memories SET state = 'expired', ",
                    "updated_at_ms = max(updated_at_ms, ?1), revision = revision + 1 ",
                    "WHERE state != 'expired' AND retention_kind = 'session' AND session_id = ?2"
                ),
                params![expired_at.get(), sqlite_integer(session_id.get())?],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(changed)
    }

    fn retrieve(
        &self,
        request: MemoryRetrievalRequest,
        cancellation: &dyn RetrievalCancellation,
    ) -> MemoryStoreResult<MemoryRetrieval> {
        check_cancellation(cancellation)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        expire_due_in(&transaction, request.now())?;
        check_cancellation(cancellation)?;
        let records = {
            let mut statement = transaction
                .prepare(&format!("{RECORD_QUERY} ORDER BY m.id LIMIT ?1"))
                .map_err(map_sqlite_error)?;
            let records = statement
                .query_map(
                    [sqlite_integer((MAX_MEMORY_SCAN_RECORDS + 1) as u64)?],
                    row_to_record,
                )
                .map_err(map_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_error)?;
            records
        };
        if records.len() > MAX_MEMORY_SCAN_RECORDS {
            return Err(store_error(
                MemoryStoreErrorKind::LimitExceeded,
                "memory retrieval scan exceeds 1024 records",
            ));
        }
        let selection = select_records(
            records,
            request.query(),
            request.now(),
            request.maximum_items(),
            request.maximum_bytes(),
            cancellation,
        )?;
        check_cancellation(cancellation)?;

        let used_bytes = selection
            .selected
            .iter()
            .map(|item| item.record.content().len())
            .sum::<usize>();
        transaction
            .execute(
                concat!(
                    "INSERT INTO retrieval_traces (turn_id, created_at_ms, maximum_items, ",
                    "maximum_bytes, selected_items, used_bytes, excluded_by_state, ",
                    "excluded_by_expiry, excluded_by_relevance, excluded_by_item_limit, ",
                    "excluded_by_byte_limit) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
                ),
                params![
                    sqlite_integer(request.turn_id().get())?,
                    request.now().get(),
                    sqlite_integer(request.maximum_items() as u64)?,
                    sqlite_integer(request.maximum_bytes() as u64)?,
                    sqlite_integer(selection.selected.len() as u64)?,
                    sqlite_integer(used_bytes as u64)?,
                    sqlite_integer(selection.exclusions.by_state() as u64)?,
                    sqlite_integer(selection.exclusions.by_expiry() as u64)?,
                    sqlite_integer(selection.exclusions.by_relevance() as u64)?,
                    sqlite_integer(selection.exclusions.by_item_limit() as u64)?,
                    sqlite_integer(selection.exclusions.by_byte_limit() as u64)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        let trace_id = checked_trace_id(transaction.last_insert_rowid())?;
        let mut context_items = Vec::with_capacity(selection.selected.len());
        let mut trace_items = Vec::with_capacity(selection.selected.len());
        for (ordinal, selected) in selection.selected.iter().enumerate() {
            check_cancellation(cancellation)?;
            transaction
                .execute(
                    concat!(
                        "INSERT INTO retrieval_items (trace_id, memory_id, ordinal, reason, content_bytes) ",
                        "VALUES (?1, ?2, ?3, ?4, ?5)"
                    ),
                    params![
                        sqlite_integer(trace_id.get())?,
                        sqlite_integer(selected.record.id().get())?,
                        sqlite_integer(ordinal as u64)?,
                        selected.reason.as_str(),
                        sqlite_integer(selected.record.content().len() as u64)?,
                    ],
                )
                .map_err(map_sqlite_error)?;
            transaction
                .execute(
                    "UPDATE memories SET last_used_at_ms = ?1, last_retrieval_reason = ?2 WHERE id = ?3",
                    params![
                        request.now().get(),
                        selected.reason.as_str(),
                        sqlite_integer(selected.record.id().get())?,
                    ],
                )
                .map_err(map_sqlite_error)?;
            context_items.push(
                MemoryContextItem::new(
                    selected.record.id(),
                    selected.record.kind(),
                    selected.record.content(),
                    selected.reason,
                )
                .map_err(|_| invalid_database_error())?,
            );
            trace_items.push(
                MemoryTraceItem::new(
                    ordinal,
                    selected.record.id(),
                    selected.record.kind(),
                    selected.reason,
                    selected.record.content().len(),
                )
                .map_err(|_| invalid_database_error())?,
            );
        }
        check_cancellation(cancellation)?;
        let trace = MemoryRetrievalTrace::new(
            trace_id,
            request.turn_id(),
            request.now(),
            trace_items,
            selection.exclusions,
        )
        .map_err(|_| invalid_database_error())?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(MemoryRetrieval::new(context_items, trace))
    }

    fn delete(&self, memory_id: MemoryId, expected_revision: u64) -> MemoryStoreResult<()> {
        if expected_revision == 0 {
            return Err(conflict_error());
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let changed = transaction
            .execute(
                "DELETE FROM memories WHERE id = ?1 AND revision = ?2",
                params![
                    sqlite_integer(memory_id.get())?,
                    sqlite_integer(expected_revision)?
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1",
                    [sqlite_integer(memory_id.get())?],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sqlite_error)?
                .is_some();
            return Err(if exists {
                conflict_error()
            } else {
                not_found_error()
            });
        }
        transaction.commit().map_err(map_sqlite_error)
    }
}

const RECORD_QUERY: &str = concat!(
    "SELECT m.id, m.kind, m.state, m.content, m.confidence, m.created_at_ms, ",
    "m.updated_at_ms, m.retention_kind, m.expires_at_ms, m.session_id, m.pinned, ",
    "m.revision, m.last_used_at_ms, m.last_retrieval_reason, s.kind, s.source_id, ",
    "s.source_timestamp_ms, s.actor, s.content_digest, a.confirmation_id, a.actor, ",
    "a.source_timestamp_ms, a.approved_revision, a.content_digest FROM memories m ",
    "JOIN memory_sources s ON s.id = (SELECT max(latest.id) FROM memory_sources latest ",
    "WHERE latest.memory_id = m.id AND latest.kind != 'user_approved') ",
    "LEFT JOIN memory_sources a ON a.id = (SELECT max(approval.id) FROM memory_sources approval ",
    "WHERE approval.memory_id = m.id AND approval.kind = 'user_approved')"
);

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let kind = parse_kind(row.get::<_, String>(1)?).map_err(store_to_sql_conversion_error)?;
    let state = parse_state(row.get::<_, String>(2)?).map_err(store_to_sql_conversion_error)?;
    let content = row.get::<_, String>(3)?;
    let created_at = checked_timestamp(row.get(5)?).map_err(store_to_sql_conversion_error)?;
    let retention = parse_retention(
        row.get::<_, String>(7)?,
        row.get(8)?,
        row.get::<_, Option<i64>>(9)?
            .map(checked_unsigned)
            .transpose()
            .map_err(store_to_sql_conversion_error)?,
    )
    .map_err(store_to_sql_conversion_error)?;
    let provenance = MemoryProvenance::new(
        parse_provenance_kind(row.get::<_, String>(14)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(15)?,
        checked_timestamp(row.get(16)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(17)?,
        row.get::<_, Option<String>>(18)?,
    )
    .map_err(protocol_to_sql_conversion_error)?;
    let approval = optional_approval(row, 19)?;
    let current_approval = (state != MemoryState::Candidate)
        .then_some(approval)
        .flatten()
        .filter(|evidence| evidence.matches_content(&content));
    let draft = MemoryDraft::new(
        kind,
        content,
        provenance,
        MemoryConfidence::new(row.get(4)?).map_err(protocol_to_sql_conversion_error)?,
        created_at,
        retention,
    )
    .map_err(protocol_to_sql_conversion_error)?;
    MemoryRecord::new(
        checked_memory_id(row.get(0)?).map_err(store_to_sql_conversion_error)?,
        draft,
        state,
        checked_timestamp(row.get(6)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, i64>(10)? == 1,
        checked_unsigned(row.get(11)?).map_err(store_to_sql_conversion_error)?,
        current_approval,
        row.get::<_, Option<i64>>(12)?
            .map(checked_timestamp)
            .transpose()
            .map_err(store_to_sql_conversion_error)?,
        row.get::<_, Option<String>>(13)?
            .map(parse_retrieval_reason)
            .transpose()
            .map_err(store_to_sql_conversion_error)?,
    )
    .map_err(protocol_to_sql_conversion_error)
}

fn insert_source(
    connection: &Connection,
    memory_id: i64,
    provenance: &MemoryProvenance,
    created_at: UnixTimestampMillis,
) -> MemoryStoreResult<()> {
    connection
        .execute(
            concat!(
                "INSERT INTO memory_sources (memory_id, kind, source_id, source_timestamp_ms, ",
                "actor, content_digest, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![
                memory_id,
                provenance.kind().as_str(),
                provenance.source_id(),
                provenance.source_timestamp().get(),
                provenance.actor(),
                provenance.content_digest(),
                created_at.get(),
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn insert_approval(
    connection: &Connection,
    memory_id: i64,
    evidence: &MemoryApprovalEvidence,
    created_at: UnixTimestampMillis,
) -> MemoryStoreResult<()> {
    connection
        .execute(
            concat!(
                "INSERT INTO memory_sources (memory_id, kind, source_id, source_timestamp_ms, actor, ",
                "content_digest, confirmation_id, approved_revision, created_at_ms) ",
                "VALUES (?1, 'user_approved', ?2, ?3, ?4, ?5, ?2, ?6, ?7)"
            ),
            params![
                memory_id,
                evidence.confirmation_id(),
                evidence.confirmed_at().get(),
                evidence.actor(),
                evidence.content_digest(),
                sqlite_integer(evidence.approved_revision())?,
                created_at.get(),
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn optional_approval(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<MemoryApprovalEvidence>> {
    let Some(confirmation_id) = row.get::<_, Option<String>>(offset)? else {
        return Ok(None);
    };
    MemoryApprovalEvidence::from_stored(
        confirmation_id,
        row.get::<_, String>(offset + 1)?,
        checked_timestamp(row.get(offset + 2)?).map_err(store_to_sql_conversion_error)?,
        checked_unsigned(row.get(offset + 3)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(offset + 4)?,
    )
    .map(Some)
    .map_err(protocol_to_sql_conversion_error)
}

fn row_to_provenance(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryProvenance> {
    MemoryProvenance::new(
        parse_provenance_kind(row.get::<_, String>(0)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(1)?,
        checked_timestamp(row.get(2)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(3)?,
        row.get::<_, Option<String>>(4)?,
    )
    .map_err(protocol_to_sql_conversion_error)
}

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryApprovalEvidence> {
    MemoryApprovalEvidence::from_stored(
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        checked_timestamp(row.get(2)?).map_err(store_to_sql_conversion_error)?,
        checked_unsigned(row.get(3)?).map_err(store_to_sql_conversion_error)?,
        row.get::<_, String>(4)?,
    )
    .map_err(protocol_to_sql_conversion_error)
}

fn bounded_history<T>(
    transaction: &rusqlite::Transaction<'_>,
    memory_id: i64,
    history_limit: usize,
    query: &str,
    mapper: for<'row> fn(&rusqlite::Row<'row>) -> rusqlite::Result<T>,
) -> MemoryStoreResult<(Vec<T>, bool)> {
    let mut statement = transaction.prepare(query).map_err(map_sqlite_error)?;
    let mut values = statement
        .query_map(
            params![memory_id, sqlite_integer((history_limit + 1) as u64)?],
            mapper,
        )
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite_error)?;
    let truncated = values.len() > history_limit;
    if truncated {
        values.pop();
    }
    values.reverse();
    Ok((values, truncated))
}

fn validate_limit(limit: usize, maximum: usize, message: &'static str) -> MemoryStoreResult<()> {
    if !(1..=maximum).contains(&limit) {
        return Err(store_error(MemoryStoreErrorKind::LimitExceeded, message));
    }
    Ok(())
}

fn expire_due(connection: &mut Connection, now: UnixTimestampMillis) -> MemoryStoreResult<usize> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let changed = expire_due_in(&transaction, now)?;
    transaction.commit().map_err(map_sqlite_error)?;
    Ok(changed)
}

fn expire_due_in(connection: &Connection, now: UnixTimestampMillis) -> MemoryStoreResult<usize> {
    connection
        .execute(
            concat!(
                "UPDATE memories SET state = 'expired', ",
                "updated_at_ms = max(updated_at_ms, ?1), revision = revision + 1 ",
                "WHERE state != 'expired' AND retention_kind IN ('working', 'until') ",
                "AND expires_at_ms <= ?1"
            ),
            [now.get()],
        )
        .map_err(map_sqlite_error)
}

fn initialize_database(database_path: &Path) -> MemoryStoreResult<()> {
    let mut connection = open_connection(database_path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    transaction
        .execute_batch(MIGRATION)
        .map_err(map_sqlite_error)?;
    transaction
        .execute(
            "INSERT INTO schema_migrations (version, checksum) VALUES (?1, ?2)",
            params![SCHEMA_VERSION, migration_checksum()],
        )
        .map_err(map_sqlite_error)?;
    transaction.commit().map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "application_id", SQLITE_APPLICATION_ID)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(map_sqlite_error)?;
    validate_database(&connection)
}

fn open_connection(database_path: &Path) -> MemoryStoreResult<Connection> {
    let connection = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(map_sqlite_error)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_error)?;
    Ok(connection)
}

fn validate_database(connection: &Connection) -> MemoryStoreResult<()> {
    let application_id: u32 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if application_id != SQLITE_APPLICATION_ID {
        return Err(invalid_database_error());
    }
    let user_version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if user_version > SCHEMA_VERSION {
        return Err(store_error(
            MemoryStoreErrorKind::UnsupportedSchema,
            "memory database schema is newer than this runtime",
        ));
    }
    if user_version != SCHEMA_VERSION {
        return Err(invalid_database_error());
    }
    let checksum = connection
        .query_row(
            "SELECT checksum FROM schema_migrations WHERE version = ?1",
            [SCHEMA_VERSION],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    let expected_checksum = migration_checksum();
    if checksum.as_deref() != Some(expected_checksum.as_str()) {
        return Err(invalid_database_error());
    }
    let foreign_key_failures: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(map_sqlite_error)?;
    if foreign_key_failures != 0 {
        return Err(invalid_database_error());
    }
    Ok(())
}

fn migration_checksum() -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in MIGRATION.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn validate_path(database_path: &Path) -> MemoryStoreResult<PathBuf> {
    if !database_path.is_absolute() {
        return Err(store_error(
            MemoryStoreErrorKind::InvalidPath,
            "memory database path must be absolute",
        ));
    }
    if fs::symlink_metadata(database_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(store_error(
            MemoryStoreErrorKind::InvalidPath,
            "memory database path must not be a symbolic link",
        ));
    }
    Ok(database_path.to_path_buf())
}

#[cfg(unix)]
fn create_database_file(database_path: &Path) -> MemoryStoreResult<()> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(database_path)
        .map_err(|_| storage_error())?;
    Ok(())
}

#[cfg(not(unix))]
fn create_database_file(database_path: &Path) -> MemoryStoreResult<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(database_path)
        .map_err(|_| storage_error())?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> MemoryStoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| storage_error())
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> MemoryStoreResult<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_file_permissions(path: &Path) -> MemoryStoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .map_err(|_| storage_error())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(store_error(
            MemoryStoreErrorKind::InvalidPath,
            "memory database permissions allow group or other access",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(_path: &Path) -> MemoryStoreResult<()> {
    Ok(())
}

fn retention_columns(
    retention: &MemoryRetention,
) -> MemoryStoreResult<(&'static str, Option<i64>, Option<i64>)> {
    Ok((
        retention.as_str(),
        retention.expires_at().map(UnixTimestampMillis::get),
        retention
            .session_id()
            .map(SessionId::get)
            .map(sqlite_integer)
            .transpose()?,
    ))
}

fn parse_retention(
    kind: String,
    expires_at: Option<i64>,
    session_id: Option<u64>,
) -> MemoryStoreResult<MemoryRetention> {
    match kind.as_str() {
        "working" => Ok(MemoryRetention::working(checked_timestamp(
            expires_at.ok_or_else(invalid_database_error)?,
        )?)),
        "session" => Ok(MemoryRetention::session(SessionId::new(
            session_id.ok_or_else(invalid_database_error)?,
        ))),
        "until" => Ok(MemoryRetention::until(checked_timestamp(
            expires_at.ok_or_else(invalid_database_error)?,
        )?)),
        "until_deleted" => Ok(MemoryRetention::UntilDeleted),
        _ => Err(invalid_database_error()),
    }
}

fn parse_kind(value: String) -> MemoryStoreResult<MemoryKind> {
    match value.as_str() {
        "working" => Ok(MemoryKind::Working),
        "episodic" => Ok(MemoryKind::Episodic),
        "semantic" => Ok(MemoryKind::Semantic),
        "identity" => Ok(MemoryKind::Identity),
        "relationship" => Ok(MemoryKind::Relationship),
        _ => Err(invalid_database_error()),
    }
}

fn parse_state(value: String) -> MemoryStoreResult<MemoryState> {
    match value.as_str() {
        "candidate" => Ok(MemoryState::Candidate),
        "active" => Ok(MemoryState::Active),
        "expired" => Ok(MemoryState::Expired),
        _ => Err(invalid_database_error()),
    }
}

fn parse_provenance_kind(value: String) -> MemoryStoreResult<MemoryProvenanceKind> {
    match value.as_str() {
        "user_provided" => Ok(MemoryProvenanceKind::UserProvided),
        "user_edited" => Ok(MemoryProvenanceKind::UserEdited),
        "completed_exchange" => Ok(MemoryProvenanceKind::CompletedExchange),
        "application_imported" => Ok(MemoryProvenanceKind::ApplicationImported),
        _ => Err(invalid_database_error()),
    }
}

fn parse_retrieval_reason(value: String) -> MemoryStoreResult<MemoryRetrievalReason> {
    match value.as_str() {
        "pinned_match" => Ok(MemoryRetrievalReason::PinnedMatch),
        "exact_phrase" => Ok(MemoryRetrievalReason::ExactPhrase),
        "shared_term" => Ok(MemoryRetrievalReason::SharedTerm),
        "recent_working" => Ok(MemoryRetrievalReason::RecentWorking),
        _ => Err(invalid_database_error()),
    }
}

fn checked_timestamp(value: i64) -> MemoryStoreResult<UnixTimestampMillis> {
    UnixTimestampMillis::new(value).map_err(|_| invalid_database_error())
}

fn checked_memory_id(value: i64) -> MemoryStoreResult<MemoryId> {
    checked_unsigned(value)
        .and_then(|value| MemoryId::new(value).map_err(|_| invalid_database_error()))
}

fn checked_trace_id(value: i64) -> MemoryStoreResult<RetrievalTraceId> {
    checked_unsigned(value)
        .and_then(|value| RetrievalTraceId::new(value).map_err(|_| invalid_database_error()))
}

fn checked_unsigned(value: i64) -> MemoryStoreResult<u64> {
    u64::try_from(value).map_err(|_| invalid_database_error())
}

fn sqlite_integer(value: u64) -> MemoryStoreResult<i64> {
    i64::try_from(value).map_err(|_| {
        store_error(
            MemoryStoreErrorKind::Conflict,
            "memory numeric value exceeds SQLite range",
        )
    })
}

fn store_to_sql_conversion_error(error: MemoryStoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn protocol_to_sql_conversion_error(error: conversation_protocol::RuntimeError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn map_sqlite_error(error: rusqlite::Error) -> MemoryStoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            store_error(MemoryStoreErrorKind::Busy, "memory database is busy")
        }
        _ => storage_error(),
    }
}

const fn invalid_database_error() -> MemoryStoreError {
    store_error(
        MemoryStoreErrorKind::InvalidDatabase,
        "memory database identity or schema is invalid",
    )
}

const fn not_found_error() -> MemoryStoreError {
    store_error(
        MemoryStoreErrorKind::NotFound,
        "memory record was not found",
    )
}

const fn conflict_error() -> MemoryStoreError {
    store_error(
        MemoryStoreErrorKind::Conflict,
        "memory record revision conflict",
    )
}

const fn storage_error() -> MemoryStoreError {
    store_error(
        MemoryStoreErrorKind::Storage,
        "memory database operation failed",
    )
}

const fn store_error(kind: MemoryStoreErrorKind, message: &'static str) -> MemoryStoreError {
    MemoryStoreError::new(kind, message)
}
