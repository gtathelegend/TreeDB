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

    /// The requested page is not resident in the buffer pool.
    #[error("Page {0} is not resident in the buffer pool")]
    PageNotResident(PageId),

    /// Attempted to unpin a page that has a pin count of zero.
    #[error("Pin count underflow for page {0}: page is already unpinned")]
    PinCountUnderflow(PageId),

    /// Buffer pool capacity must be greater than zero.
    #[error("Invalid buffer pool capacity: {0} (must be greater than 0)")]
    InvalidCapacity(usize),

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

    /// A duplicate key was rejected during B+ Tree insertion.
    #[error("Duplicate key error: key {0} already exists in B+ Tree")]
    DuplicateKey(u64),

    /// Recovery operation was interrupted by simulated fault injection.
    #[error("Recovery interrupted: {0}")]
    RecoveryInterrupted(String),

    /// WAL corruption detected during recovery scan.
    #[error("WAL corruption detected: {0}")]
    WalCorruption(String),

    /// Attempted to free a permanently reserved page (e.g. Page 0 Header or Page 1 Metadata).
    #[error("Cannot free reserved page {0}")]
    CannotFreeReservedPage(PageId),

    /// Attempted to free a page that is already marked as free.
    #[error("Page {0} is already free")]
    PageAlreadyFree(PageId),

    /// Structural corruption or invariant violation in the freelist.
    #[error("Freelist corruption detected: {0}")]
    FreelistCorruption(String),

    /// Structural corruption or invariant violation in an overflow chain.
    #[error("Overflow corruption detected: {0}")]
    OverflowCorruption(String),
}
