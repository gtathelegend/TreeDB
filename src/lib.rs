//! TreeDB: A persistent single-node storage engine built from first principles in Rust.

pub mod engine;
pub mod error;
pub mod index;
pub mod query;
pub mod sql;
pub mod storage;
pub mod wal;

pub use engine::{
    Catalog, CatalogPage, Column, DataType, IndexId, IndexMetadata, Row, Schema, StatementResult,
    Table, TableId, TableMetadata, Transaction, TreeDb, TxnState, Value, CATALOG_MAGIC,
    CURRENT_CATALOG_VERSION, MAX_COLUMN_NAME_LEN, MAX_INDEX_NAME_LEN, MAX_TABLE_NAME_LEN,
    TUPLE_FORMAT_VERSION,
};
pub use error::{Error, Result};
pub use index::{
    BPlusTree, IndexKey, InternalNode, LeafNode, RecordId, SecondaryIndex, SecondaryInternalNode,
    SecondaryLeafNode, INTERNAL_ENTRY_SIZE, INTERNAL_HEADER_SIZE, INTERNAL_MAX_KEYS,
    INTERNAL_MIN_CHILDREN, INTERNAL_MIN_KEYS, INVALID_PAGE_ID, KEY_SIZE, LEAF_ENTRY_SIZE,
    LEAF_HEADER_SIZE, LEAF_MAX_ENTRIES, LEAF_MIN_ENTRIES, PAGE_ID_SIZE, RECORD_ID_SIZE,
    SEC_INTERNAL_HEADER_SIZE, SEC_LEAF_HEADER_SIZE,
};
pub use query::{
    BinaryOp, Expr, FilterOperator, OffsetLimitOperator, ProjectOperator, Query, QueryResult,
    SortDirection, SortKey, SortOperator, UnaryOp,
};
pub use sql::{
    bind_select, parse as parse_sql, parse_statement as parse_statement_sql,
    tokenize as tokenize_sql, BindingContext, BoundSelect, ColumnDef, CreateTableStatement,
    DeleteStatement, DropTableStatement, InsertStatement, Lexer, OrderByExpr, OrderDirection,
    Parser, SelectItem, SelectStatement, SourceSpan, SqlBinaryOp, SqlExpr, SqlLiteral, SqlUnaryOp,
    Statement, TableRef, Token, TokenKind, UpdateAssignment, UpdateStatement, MAX_IDENTIFIER_LEN,
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
