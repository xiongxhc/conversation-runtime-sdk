use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryStoreErrorKind {
    InvalidPath,
    NotInitialized,
    UnsupportedSchema,
    InvalidDatabase,
    NotFound,
    Conflict,
    Busy,
    Cancelled,
    LimitExceeded,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryStoreError {
    kind: MemoryStoreErrorKind,
    message: &'static str,
}

impl MemoryStoreError {
    pub(crate) const fn new(kind: MemoryStoreErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub const fn kind(&self) -> MemoryStoreErrorKind {
        self.kind
    }

    pub const fn cancelled() -> Self {
        Self::new(
            MemoryStoreErrorKind::Cancelled,
            "memory retrieval was cancelled",
        )
    }
}

impl fmt::Display for MemoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for MemoryStoreError {}

pub type MemoryStoreResult<T> = Result<T, MemoryStoreError>;
