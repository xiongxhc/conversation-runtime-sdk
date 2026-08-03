use conversation_protocol::{
    MemoryApproval, MemoryDraft, MemoryId, MemoryInspection, MemoryPatch, MemoryRecord,
    MemoryRetrievalRequest, SessionId, UnixTimestampMillis,
};

use crate::{MemoryRetrieval, MemoryStoreResult, RetrievalCancellation};

pub trait MemoryStore: Send + Sync {
    fn create(&self, draft: MemoryDraft) -> MemoryStoreResult<MemoryRecord>;

    fn list(&self, now: UnixTimestampMillis) -> MemoryStoreResult<Vec<MemoryRecord>>;

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
