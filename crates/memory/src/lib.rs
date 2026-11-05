mod error;
mod people;
mod provider;
mod retrieval;
mod signatures;
mod sqlite;
mod store;

pub use error::{MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult};
pub use people::{
    NewPerson, PeopleStore, Person, PersonMaturity, PersonName, MAX_NAMES_PER_PERSON, MAX_PEOPLE,
};
pub use provider::{
    MemoryClock, MemoryContextProvider, MemoryProviderFuture, SqliteMemoryContextProvider,
    SystemMemoryClock,
};
pub use retrieval::{MemoryRetrieval, NeverCancelled, RetrievalCancellation};
pub use signatures::{
    SignatureId, SignatureKey, SignatureModel, SignatureRole, VoiceSignature, VoiceSignatureDraft,
    MAX_SIGNATURES_PER_PERSON,
};
pub use sqlite::{SqliteMemoryStore, SCHEMA_VERSION, SQLITE_APPLICATION_ID};
pub use store::{BoundedMemoryInspection, MemoryPage, MemoryStore};

pub const MAX_MEMORY_SCAN_RECORDS: usize = 1_024;
