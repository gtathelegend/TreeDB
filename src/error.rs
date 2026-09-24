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

    /// Attempted to mutate or commit an already committed transaction.
    #[error("Transaction {0} has already been committed")]
    TransactionAlreadyCommitted(crate::wal::lsn::TxnId),

    /// Attempted to mutate, commit, or abort an already aborted transaction.
    #[error("Transaction {0} has already been aborted")]
    TransactionAlreadyAborted(crate::wal::lsn::TxnId),

    /// Transaction is not in active state.
    #[error("Transaction {0} is not active")]
    TransactionNotActive(crate::wal::lsn::TxnId),

    /// Attempted to begin a transaction while another is already active.
    #[error("A transaction ({0}) is already active on the engine")]
    TransactionActive(crate::wal::lsn::TxnId),

    /// Table with the specified name already exists in the catalog.
    #[error("Table '{0}' already exists")]
    TableAlreadyExists(String),

    /// Table with the specified name was not found in the catalog.
    #[error("Table '{0}' not found")]
    TableNotFound(String),

    /// Table name is invalid (e.g. empty, too long, or invalid characters).
    #[error("Invalid table name: {0}")]
    InvalidTableName(String),

    /// Catalog structure or metadata corruption detected.
    #[error("Catalog corruption detected: {0}")]
    CatalogCorruption(String),

    /// Catalog version is unsupported by the current engine.
    #[error("Unsupported catalog version: found {found}, supported {supported}")]
    CatalogVersionUnsupported { found: u32, supported: u32 },

    /// Invalid or out-of-bounds table identifier.
    #[error("Invalid table ID: {0}")]
    InvalidTableId(u32),

    /// Table schema definition is invalid or inconsistent.
    #[error("Table schema error: {0}")]
    TableSchemaMismatch(String),

    /// Row column count does not match table schema column count.
    #[error("Column count mismatch: expected {expected}, found {found}")]
    ColumnCountMismatch { expected: usize, found: usize },

    /// Row value data type does not match column schema.
    #[error("Type mismatch for column '{column}': expected {expected}, found {found}")]
    TypeMismatch {
        column: String,
        expected: String,
        found: String,
    },

    /// Non-nullable column received a NULL value.
    #[error("NULL constraint violation for column '{column}'")]
    NullConstraintViolation { column: String },

    /// Primary key value already exists in the table.
    #[error("Duplicate primary key: {0}")]
    DuplicatePrimaryKey(String),

    /// Row with the specified primary key was not found.
    #[error("Row not found")]
    RowNotFound,

    /// Invalid primary key value or definition.
    #[error("Invalid primary key: {0}")]
    InvalidPrimaryKey(String),

    /// Binary tuple corruption detected during decoding.
    #[error("Tuple corruption detected: {0}")]
    TupleCorruption(String),

    /// Serialized tuple format version is unsupported.
    #[error("Unsupported tuple format version: found {found}, supported {supported}")]
    TupleVersionUnsupported { found: u8, supported: u8 },

    /// Column with the specified name was not found in schema.
    #[error("Column '{0}' not found")]
    ColumnNotFound(String),

    /// Expression type checking failed with mismatched types.
    #[error("Expression type mismatch: expected {expected}, found {found}")]
    ExpressionTypeMismatch { expected: String, found: String },

    /// Comparison operator is invalid for the operand types.
    #[error("Invalid comparison between {left} and {right} using operator '{op}'")]
    InvalidComparison {
        left: String,
        right: String,
        op: String,
    },

    /// Boolean operator received a non-boolean operand.
    #[error("Invalid boolean expression: operand type is {found}")]
    InvalidBooleanExpression { found: String },

    /// Sort key expression is invalid.
    #[error("Invalid sort expression: {0}")]
    InvalidSortExpression(String),

    /// Projection expression list is invalid.
    #[error("Invalid projection: {0}")]
    InvalidProjection(String),

    /// General query execution error.
    #[error("Query execution error: {0}")]
    QueryExecutionError(String),

    /// Lexical error encountered during SQL tokenization.
    #[error("SQL lexical error at position {start}..{end}: {message}")]
    SqlLexError {
        message: String,
        start: usize,
        end: usize,
    },

    /// Syntax error encountered during SQL parsing.
    #[error("SQL syntax error at position {start}..{end}: {message}")]
    SqlParseError {
        message: String,
        start: usize,
        end: usize,
    },

    /// Semantic error encountered during SQL name binding and validation.
    #[error("SQL binding error: {0}")]
    SqlBindingError(String),

    /// Index with the specified name was not found in the catalog.
    #[error("Index '{0}' not found")]
    IndexNotFound(String),

    /// Index with the specified name already exists in the catalog.
    #[error("Index '{0}' already exists")]
    DuplicateIndexName(String),

    /// Duplicate key value rejected by unique secondary index constraint.
    #[error("Duplicate index key: {0}")]
    DuplicateIndexKey(String),

    /// Column specified for index creation was not found on table.
    #[error("Index column '{0}' not found in table schema")]
    IndexColumnNotFound(String),

    /// Secondary index definition is invalid.
    #[error("Invalid index definition: {0}")]
    InvalidIndexDefinition(String),

    /// Structural corruption detected in secondary index.
    #[error("Index corruption detected: {0}")]
    IndexCorruption(String),

    /// Index does not belong to the expected table.
    #[error("Index '{index}' does not belong to table '{table}'")]
    IndexTableMismatch { index: String, table: String },

    /// Index identifier is invalid or out of bounds.
    #[error("Invalid index ID: {0}")]
    InvalidIndexId(u32),

    /// Index name is invalid (e.g. empty, too long).
    #[error("Invalid index name: {0}")]
    InvalidIndexName(String),

    /// Column in SELECT or HAVING is not in GROUP BY and not in an aggregate function.
    #[error("Column '{0}' must appear in the GROUP BY clause or be used in an aggregate function")]
    UngroupedColumn(String),

    /// Aggregate functions are not allowed in WHERE clause.
    #[error("Aggregate functions are not allowed in WHERE clause")]
    AggregateInWhere,

    /// Invalid expression in HAVING clause.
    #[error("Invalid HAVING expression: {0}")]
    InvalidHavingExpression(String),

    /// Invalid argument provided to aggregate function.
    #[error("Invalid aggregate argument: {0}")]
    InvalidAggregateArgument(String),

    /// Aggregate nesting is not supported.
    #[error("Nested aggregate functions are not allowed: {0}")]
    NestedAggregate(String),

    /// Aggregate function called on unsupported data type.
    #[error("Aggregate function '{func}' is not supported for data type '{data_type}'")]
    UnsupportedAggregateType {
        /// Aggregate function name.
        func: String,
        /// Data type encountered.
        data_type: String,
    },

    /// Arithmetic overflow during aggregate accumulator evaluation.
    #[error("Arithmetic overflow during aggregate calculation: {0}")]
    AggregateOverflow(String),

    /// Unknown or unsupported aggregate function name.
    #[error("Unknown aggregate function '{0}'")]
    UnknownAggregateFunction(String),

    /// Invalid GROUP BY expression.
    #[error("Invalid GROUP BY expression: {0}")]
    InvalidGroupByExpression(String),
}
