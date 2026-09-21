//! TreeDB: A persistent single-node storage engine built from first principles in Rust.

pub mod error;
pub mod storage;

pub use error::{Error, Result};
pub use storage::{
    DbHeader, Key, Page, PageId, PageManager, PageType, CURRENT_FORMAT_VERSION,
    DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID, MAGIC_BYTES, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE,
    PAGE_SIZE,
};
