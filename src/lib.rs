//! TreeDB: A persistent single-node storage engine built from first principles in Rust.

pub mod engine;
pub mod error;
pub mod index;
pub mod query;
pub mod sql;
pub mod storage;
pub mod wal;

pub use engine::{
    resolve_source_wal_path, BackupConfig, BackupError, BackupManifest, BackupReport,
    BackupValidationReport, Catalog, CatalogPage, Column, CommitSeq, ConsistencyCheckConfig,
    ConsistencyIssue, ConsistencyIssueKind, ConsistencyReport, ConsistencySeverity,
    ConsistencyStatistics, Crc32Hasher, CurrentVersionVisibility, DataType, IndexId, IndexMetadata,
    IsolationContext, IsolationLevel, IsolationStatus, MultiIndexScanStats, ReadSet,
    ReclamationHorizon, RestoreConfig, RestoreReport, Row, Schema, Snapshot, SnapshotId,
    SnapshotRegistry, StatementResult, StorageFailurePoint, Table, TableId, TableMetadata,
    Transaction, TransactionCommitMetadata, TransactionWriteSet, TreeDb, TxnState, VacuumConfig,
    VacuumResult, Value, VersionReclamationEligibility, VisibilityPolicy, BACKUP_MANIFEST_MAGIC,
    CATALOG_MAGIC, CURRENT_BACKUP_VERSION, CURRENT_CATALOG_VERSION, MANIFEST_FILE_NAME,
    MAX_COLUMN_NAME_LEN, MAX_INDEX_NAME_LEN, MAX_TABLE_NAME_LEN, TUPLE_FORMAT_VERSION,
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
    assert_differential_equivalence, compute_estimation_error, estimate_error_ratio,
    AdaptivePlanSelector, AdaptiveSelectionConfig, AdaptiveSelectionDiagnostics,
    AdaptiveSelectionMetrics, AdaptiveSelectionResult, AggregateConfig, AggregateExecutionMetrics,
    AggregateOperator, AggregateState, AggregateStrategy, BestPlan, BinaryOp, CanonicalRow,
    CardinalityEstimator, ColumnStatistics, CostModel, DifferentialFailureReport, ExecutionContext,
    ExecutionOperator, ExecutionResourceLimits, ExecutionResourceManager,
    ExecutionTerminationStatus, Expr, ExternalAggregator, ExternalHashJoiner, ExternalSorter,
    FeedbackAction, FeedbackConfidence, FeedbackContext, FeedbackEntry, FeedbackKey,
    FeedbackObservation, FeedbackRegistry, FilterOperator, GroupId, IndexStatistics, JoinConfig,
    JoinExecutionMetrics, JoinKey, LogicalOp, LogicalSubplanFingerprint, MemoConfig, MemoGroup,
    MemoStats, MultiIndexChildScan, MultiIndexConfig, MultiIndexExecutionMode,
    MultiIndexSpillMetrics, MultiIndexStrategy, OffsetLimitOperator, OperatorCounters,
    OperatorKind, OperatorLifecycleState, OperatorMetrics, OptimizerMemo, OrderingColumn,
    OrderingProperty, ParamPlanningSignature, PhysicalAlternative, PhysicalPlanExecutor,
    PhysicalPlanFingerprint, PhysicalPlanValidator, PhysicalProperties, PhysicalPropertyKey,
    PlanCache, PlanCacheEntry, PlanCacheKey, PlanCacheStats, PlanCandidate, PlanCandidateSet,
    PlanCost, PlanLifecycleState, PlanObservation, PlanValidationContext, PlanValidationError,
    PlanValidity, ProjectOperator, Query, QueryExecutionMetrics, QueryResult, ReferenceEvaluator,
    ReplanDecision, RequiredColumns, RequiredOrdering, RequiredPhysicalProperties,
    ResourceFeedback, ResourceObservation, ResourcePlanningSignature, RowGoal, RunReader,
    SortConfig, SortDirection, SortExecutionMetrics, SortKey, SortOperator, SpillStatus,
    StatisticsProvider, StatsRegistry, StreamingAggregateMetrics, StreamingAggregateOperator,
    TableStatistics, TableValidationInfo, TempAggregateFile, TempJoinFile, TempMultiIndexRunFile,
    TempRunFile, TopNExecutionMetrics, TopNOperator, UnaryOp, ValidatedPhysicalPlan,
    DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES, DEFAULT_FEEDBACK_ALPHA, DEFAULT_FEEDBACK_CAPACITY,
    DEFAULT_JOIN_MEMORY_BUDGET_BYTES, DEFAULT_MAX_CANDIDATES, DEFAULT_MAX_COST_ADJUSTMENT_RATIO,
    DEFAULT_MAX_MERGE_PASSES, DEFAULT_MAX_TEMP_BYTES, DEFAULT_MAX_TEMP_FILES,
    DEFAULT_MIN_CONFIDENCE, DEFAULT_MIN_OBSERVATIONS, DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES,
    DEFAULT_PLAN_CACHE_CAPACITY, DEFAULT_REPLAN_MIN_OBSERVATIONS, DEFAULT_REPLAN_THRESHOLD_RATIO,
    DEFAULT_SORT_MEMORY_BUDGET_BYTES, DEFAULT_SWITCH_THRESHOLD, MAX_MULTI_INDEXES,
    OUTLIER_MAX_RATIO, OUTLIER_MIN_RATIO, PHYSICAL_PLAN_FINGERPRINT_VERSION,
    RECORD_ID_SET_OP_CPU_COST, SKIP_SCAN_PROBE_COST,
};

pub use sql::{
    bind_select, parse as parse_sql, parse_statement as parse_statement_sql,
    tokenize as tokenize_sql, AnalyzeStatement, BindingContext, BoundSelect, ColumnDef,
    CreateIndexStatement, CreateTableStatement, DeleteStatement, DropIndexStatement,
    DropTableStatement, ExplainStatement, InsertStatement, Lexer, OrderByExpr, OrderDirection,
    ParameterMetadata, Parser, PreparedStatement, SelectItem, SelectStatement, SourceSpan,
    SqlBinaryOp, SqlExpr, SqlLiteral, SqlUnaryOp, Statement, TableRef, Token, TokenKind,
    UpdateAssignment, UpdateStatement, MAX_IDENTIFIER_LEN,
};

pub use storage::{
    is_reserved_page, BufferPool, BufferPoolStats, DbHeader, FrameId, FreePageLayout,
    FreePageManager, Key, OverflowDescriptor, OverflowPage, Page, PageId, PageManager, PageType,
    RecordLocation, RecordPage, RecordStore, Slot, VersionId, VersionMetadata,
    CURRENT_FORMAT_VERSION, DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID, MAGIC_BYTES, MAX_RECORD_SIZE,
    MAX_VERSION_CHAIN_DEPTH, MVCC_VERSION_FORMAT, NO_FREE_PAGE, NO_OVERFLOW_PAGE,
    OVERFLOW_DESCRIPTOR_SIZE, OVERFLOW_FLAG, OVERFLOW_HEADER_SIZE, OVERFLOW_PAYLOAD_SIZE,
    PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE, RECORD_PAGE_HEADER_SIZE, SLOT_SIZE,
    VERSION_HEADER_SIZE,
};

pub use wal::{
    crc32, LogManager, LogRecord, LogRecordBody, LogRecordType, Lsn, RecoveryEngine, RecoveryFault,
    RecoveryStats, TxnId, TxnStatus, TxnTableEntry, WalHeader, CURRENT_WAL_VERSION, INVALID_LSN,
    NULL_LSN, NULL_TXN, WAL_HEADER_SIZE, WAL_MAGIC_BYTES,
};
