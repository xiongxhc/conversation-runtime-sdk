use conversation_protocol::{
    MemoryApproval, MemoryDraft, MemoryId, MemoryInspection, MemoryPatch, MemoryRecord,
    MemoryRetrievalRequest, SessionId, UnixTimestampMillis,
};

use crate::{MemoryRetrieval, MemoryStoreResult, RetrievalCancellation};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryPage {
    records: Vec<MemoryRecord>,
    next_before_id: Option<MemoryId>,
}

impl MemoryPage {
    pub(crate) fn new(records: Vec<MemoryRecord>, next_before_id: Option<MemoryId>) -> Self {
        Self {
            records,
            next_before_id,
        }
    }

    pub fn records(&self) -> &[MemoryRecord] {
        &self.records
    }

    pub const fn next_before_id(&self) -> Option<MemoryId> {
        self.next_before_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedMemoryInspection {
    inspection: MemoryInspection,
    sources_truncated: bool,
    approvals_truncated: bool,
}

impl BoundedMemoryInspection {
    pub(crate) fn new(
        inspection: MemoryInspection,
        sources_truncated: bool,
        approvals_truncated: bool,
    ) -> Self {
        Self {
            inspection,
            sources_truncated,
            approvals_truncated,
        }
    }

    pub const fn inspection(&self) -> &MemoryInspection {
        &self.inspection
    }

    pub const fn sources_truncated(&self) -> bool {
        self.sources_truncated
    }

    pub const fn approvals_truncated(&self) -> bool {
        self.approvals_truncated
    }
}

pub trait MemoryStore: Send + Sync {
    fn create(&self, draft: MemoryDraft) -> MemoryStoreResult<MemoryRecord>;

    fn list(&self, now: UnixTimestampMillis) -> MemoryStoreResult<Vec<MemoryRecord>>;

    fn list_page(
        &self,
        now: UnixTimestampMillis,
        before_id: Option<MemoryId>,
        limit: usize,
    ) -> MemoryStoreResult<MemoryPage>;

    fn inspect(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryRecord>;

    fn inspect_with_sources(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryInspection>;

    fn inspect_bounded(
        &self,
        memory_id: MemoryId,
        now: UnixTimestampMillis,
        history_limit: usize,
    ) -> MemoryStoreResult<BoundedMemoryInspection>;

    fn edit(&self, memory_id: MemoryId, patch: MemoryPatch) -> MemoryStoreResult<MemoryRecord>;

    fn approve(
        &self,
        memory_id: MemoryId,
        approval: MemoryApproval,
    ) -> MemoryStoreResult<MemoryRecord>;

    fn set_pinned(
        &self,
        memory_id: MemoryId,
        expected_revision: u64,
        pinned: bool,
        changed_at: UnixTimestampMillis,
    ) -> MemoryStoreResult<MemoryRecord>;

    fn prune_expired(&self, now: UnixTimestampMillis) -> MemoryStoreResult<usize>;

    fn expire_session(
        &self,
        session_id: SessionId,
        expired_at: UnixTimestampMillis,
    ) -> MemoryStoreResult<usize>;

    fn retrieve(
        &self,
        request: MemoryRetrievalRequest,
        cancellation: &dyn RetrievalCancellation,
    ) -> MemoryStoreResult<MemoryRetrieval>;

    fn delete(&self, memory_id: MemoryId, expected_revision: u64) -> MemoryStoreResult<()>;
}
