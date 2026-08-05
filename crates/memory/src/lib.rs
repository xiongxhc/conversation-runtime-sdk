mod error;
mod provider;
mod retrieval;
mod sqlite;
mod store;

pub use error::{MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult};
pub use provider::{
    MemoryClock, MemoryContextProvider, MemoryProviderFuture, SqliteMemoryContextProvider,
    SystemMemoryClock,
};
pub use retrieval::{MemoryRetrieval, NeverCancelled, RetrievalCancellation};
pub use sqlite::{SqliteMemoryStore, SCHEMA_VERSION, SQLITE_APPLICATION_ID};
pub use store::{BoundedMemoryInspection, MemoryPage, MemoryStore};

pub const MAX_MEMORY_SCAN_RECORDS: usize = 1_024;
