use thiserror::Error;

use crate::storage::page::PageId;

/// Result alias for TreeDB operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Structured errors returned by TreeDB operations.
#[derive(Error, Debug)]
pub enum Error {
    /// Underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The database file header or magic bytes are invalid.
    #[error("Invalid database: {0}")]
    InvalidDatabase(String),

    /// The database file was created with an unsupported format version.
    #[error("Unsupported format version: found {found}, supported {supported}")]
    UnsupportedFormatVersion { found: u32, supported: u32 },

    /// The requested page ID is invalid or out of bounds.
    #[error("Invalid page ID {0}")]
    InvalidPage(PageId),

    /// Page contains corrupt data or fails structural validation.
    #[error("Page corruption detected on page {page_id}: {reason}")]
    PageCorruption {
        page_id: PageId,
        reason: &'static str,
    },

    /// The requested page was not found.
    #[error("Page not found: {0}")]
    PageNotFound(PageId),

    /// No unpinned/evictable page available in the buffer pool.
    #[error("No evictable page available in buffer pool")]
    NoEvictablePage,

    /// Buffer pool has reached maximum capacity with all frames pinned.
    #[error("Buffer pool full")]
    BufferPoolFull,

    /// Value exceeds maximum allowable size for the target page.
    #[error("Value too large: size {size} bytes exceeds limit of {limit} bytes")]
    ValueTooLarge { size: usize, limit: usize },

    /// Serialization or deserialization failure.
    #[error("Serialization failure: {0}")]
    SerializationFailure(String),

    /// Invariant violation or corruption within the B+ Tree structure.
    #[error("Tree corruption detected: {0}")]
    TreeCorruption(String),
}
