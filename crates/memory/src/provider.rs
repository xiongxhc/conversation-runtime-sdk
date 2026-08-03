use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use conversation_protocol::{
    ExecutionLocation, MemoryRetrievalRequest, TurnId, UnixTimestampMillis,
    MAX_MEMORY_RETRIEVAL_BYTES, MAX_MEMORY_RETRIEVAL_ITEMS,
};
use tokio_util::sync::CancellationToken;

use crate::{
    MemoryRetrieval, MemoryStore, MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult,
    RetrievalCancellation, SqliteMemoryStore,
};

const DEFAULT_MEMORY_ITEMS: usize = 4;
const DEFAULT_MEMORY_BYTES: usize = 4 * 1024;

pub type MemoryProviderFuture<'a> =
    Pin<Box<dyn Future<Output = MemoryStoreResult<MemoryRetrieval>> + Send + 'a>>;

pub trait MemoryClock: Send + Sync {
    fn now(&self) -> MemoryStoreResult<UnixTimestampMillis>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMemoryClock;

impl MemoryClock for SystemMemoryClock {
    fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| storage_error())?;
        let millis = i64::try_from(elapsed.as_millis()).map_err(|_| storage_error())?;
        UnixTimestampMillis::new(millis).map_err(|_| storage_error())
    }
}

pub trait MemoryContextProvider: Send + Sync {
    fn execution_location(&self) -> ExecutionLocation;

    fn retrieve(
        &self,
        turn_id: TurnId,
        query: String,
        cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_>;
}

#[derive(Clone)]
pub struct SqliteMemoryContextProvider {
    store: SqliteMemoryStore,
    clock: Arc<dyn MemoryClock>,
    maximum_items: usize,
    maximum_bytes: usize,
}

impl SqliteMemoryContextProvider {
    pub fn new(store: SqliteMemoryStore, clock: Arc<dyn MemoryClock>) -> Self {
        Self {
            store,
            clock,
            maximum_items: DEFAULT_MEMORY_ITEMS,
            maximum_bytes: DEFAULT_MEMORY_BYTES,
        }
    }

    pub fn with_limits(
        mut self,
        maximum_items: usize,
        maximum_bytes: usize,
    ) -> MemoryStoreResult<Self> {
        if !(1..=MAX_MEMORY_RETRIEVAL_ITEMS).contains(&maximum_items)
            || !(1..=MAX_MEMORY_RETRIEVAL_BYTES).contains(&maximum_bytes)
        {
            return Err(MemoryStoreError::new(
                MemoryStoreErrorKind::Conflict,
                "memory provider limits are invalid",
            ));
        }
        self.maximum_items = maximum_items;
        self.maximum_bytes = maximum_bytes;
        Ok(self)
    }
}

impl MemoryContextProvider for SqliteMemoryContextProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Local
    }

    fn retrieve(
        &self,
        turn_id: TurnId,
        query: String,
        cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        let store = self.store.clone();
        let clock = Arc::clone(&self.clock);
        let maximum_items = self.maximum_items;
        let maximum_bytes = self.maximum_bytes;
        Box::pin(async move {
            let operation = tokio::task::spawn_blocking(move || {
                let request = MemoryRetrievalRequest::new(
                    turn_id,
                    query,
                    clock.now()?,
                    maximum_items,
                    maximum_bytes,
                )
                .map_err(|_| {
                    MemoryStoreError::new(
                        MemoryStoreErrorKind::Conflict,
                        "memory retrieval request is invalid",
                    )
                })?;
                store.retrieve(request, &TokenCancellation(cancellation))
            });
            operation.await.map_err(|_| storage_error())?
        })
    }
}

struct TokenCancellation(CancellationToken);

impl RetrievalCancellation for TokenCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

const fn storage_error() -> MemoryStoreError {
    MemoryStoreError::new(
        MemoryStoreErrorKind::Storage,
        "memory provider operation failed",
    )
}
