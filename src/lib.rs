//! TreeDB: A persistent single-node storage engine built from first principles in Rust.

pub mod engine;
pub mod error;
pub mod index;
pub mod storage;
pub mod wal;

pub use engine::TreeDb;
pub use error::{Error, Result};
pub use index::{
    BPlusTree, InternalNode, LeafNode, RecordId, INTERNAL_ENTRY_SIZE, INTERNAL_HEADER_SIZE,
    INTERNAL_MAX_KEYS, INTERNAL_MIN_CHILDREN, INTERNAL_MIN_KEYS, INVALID_PAGE_ID, KEY_SIZE,
    LEAF_ENTRY_SIZE, LEAF_HEADER_SIZE, LEAF_MAX_ENTRIES, LEAF_MIN_ENTRIES, PAGE_ID_SIZE,
    RECORD_ID_SIZE,
};
pub use storage::{
    is_reserved_page, BufferPool, BufferPoolStats, DbHeader, FrameId, FreePageLayout,
    FreePageManager, Key, OverflowDescriptor, OverflowPage, Page, PageId, PageManager, PageType,
    RecordLocation, RecordPage, RecordStore, Slot, CURRENT_FORMAT_VERSION,
    DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID, MAGIC_BYTES, MAX_RECORD_SIZE, NO_FREE_PAGE,
    NO_OVERFLOW_PAGE, OVERFLOW_DESCRIPTOR_SIZE, OVERFLOW_FLAG, OVERFLOW_HEADER_SIZE,
    OVERFLOW_PAYLOAD_SIZE, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE, RECORD_PAGE_HEADER_SIZE,
    SLOT_SIZE,
};

pub use wal::{
    crc32, LogManager, LogRecord, LogRecordBody, LogRecordType, Lsn, RecoveryEngine, RecoveryFault,
    RecoveryStats, TxnId, TxnStatus, TxnTableEntry, WalHeader, CURRENT_WAL_VERSION, INVALID_LSN,
    NULL_LSN, NULL_TXN, WAL_HEADER_SIZE, WAL_MAGIC_BYTES,
};
