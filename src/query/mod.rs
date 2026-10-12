pub mod builder;
pub mod cardinality;
pub mod cost;
pub mod execution;
pub mod expr;
pub mod external_aggregate;
pub mod external_hash_join;
pub mod external_multi_index;
pub mod external_sort;
pub mod feedback;
pub mod fingerprint;
pub mod inference;
pub mod lifecycle;
pub mod materialize;
pub mod memo;
pub mod merge_join;
pub mod metrics;
pub mod operator;
pub mod optimizer;
pub mod physical_properties;
pub mod plan_cache;
pub mod reference;
pub mod rewrite;
pub mod runtime_filter;
pub mod stats;
pub mod streaming_aggregate;
pub mod top_n;
pub mod validation;

pub use memo::{
    BestPlan, GroupId, LogicalOp, MemoConfig, MemoGroup, MemoStats, OptimizerMemo,
    PhysicalAlternative, PhysicalPropertyKey,
};
pub use optimizer::AggregateStrategy;
pub use physical_properties::{
    ordering_property_for_table_scan, OrderingColumn, OrderingProperty, PhysicalProperties,
    RequiredColumns, RequiredOrdering, RequiredPhysicalProperties, RowGoal,
};
pub use streaming_aggregate::{StreamingAggregateMetrics, StreamingAggregateOperator};
pub use top_n::{TopNExecutionMetrics, TopNOperator};

pub use merge_join::MergeJoinOperator;

pub use runtime_filter::{
    should_build_runtime_filter, BloomRuntimeFilter, ExactRuntimeFilter, RuntimeFilter,
    RuntimeFilterPlan, RuntimeFilterScope, RuntimeFilterType,
};

pub use inference::{
    ColumnConstraint, ColumnRange, ConstraintSet, InferenceMetrics, InferenceSource,
    InferredPredicate, RangeBound, RuleSafety, MAX_PROPAGATION_ROUNDS,
};

pub use builder::Query;
pub use cardinality::{
    CardinalityEstimator, DEFAULT_EQ_SELECTIVITY, DEFAULT_NULL_FRACTION, DEFAULT_RANGE_SELECTIVITY,
    DEFAULT_TABLE_ROWS,
};
pub use cost::{
    CostModel, PlanCost, AGG_CPU_COST, BPLUS_TREE_HEIGHT_COST, FILTER_CPU_COST, JOIN_CPU_COST,
    PAGE_READ_COST, PK_LOOKUP_COST, RECORD_ID_SET_OP_CPU_COST, ROW_CPU_COST, SEC_INDEX_FETCH_COST,
    SEC_INDEX_LOOKUP_COST, SKIP_SCAN_PROBE_COST, SORT_CPU_COST, TUPLE_FETCH_COST, WINDOW_CPU_COST,
};
pub use execution::{
    safe_close, validate_transition, ExecutionContext, ExecutionOperator, ExecutionResourceLimits,
    ExecutionResourceManager, ExecutionTerminationStatus, FilterExecutionOperator,
    MaterializedResultOperator, OffsetLimitExecutionOperator, OperatorLifecycleState,
    PhysicalPlanExecutor, PhysicalScanOperator, ProjectExecutionOperator, SortExecutionOperator,
    TopNExecutionOperator,
};
pub use expr::{
    AggregateExpr, AggregateFunction, BinaryOp, BoundWindowFrame, BoundWindowSpec, Expr, UnaryOp,
    WindowExpr,
};
pub use external_aggregate::{
    estimate_group_entry_memory, hash_group_key, AggregateConfig, AggregateExecutionMetrics,
    ExternalAggregator, TempAggregateFile, DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES, TEMP_AGG_MAGIC,
    TEMP_AGG_VERSION,
};
pub use external_hash_join::{
    analyze_equi_join, hash_join_key, EquiJoinPlan, ExternalHashJoiner, JoinConfig,
    JoinExecutionMetrics, JoinKey, TempJoinFile, DEFAULT_JOIN_MEMORY_BUDGET_BYTES, TEMP_JOIN_MAGIC,
    TEMP_JOIN_VERSION,
};
pub use external_multi_index::{
    execute_multi_index_intersection_streams, execute_multi_index_union_streams,
    prepare_child_candidate_stream, KWayRecordIdMerger, MultiIndexConfig, MultiIndexExecutionMode,
    MultiIndexSpillMetrics, RecordIdRunReader, RecordIdRunWriter, RecordIdStream, SpillOpType,
    TempMultiIndexRunFile, DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES, RECORD_ID_IN_MEMORY_BYTES,
    RECORD_ID_PAYLOAD_BYTES, TEMP_MULTI_INDEX_MAGIC, TEMP_MULTI_INDEX_VERSION,
};
pub use external_sort::{
    estimate_row_memory, write_sorted_run, ExternalSorter, RunReader, SortConfig,
    SortExecutionMetrics, TempRunFile, DEFAULT_MAX_MERGE_PASSES, DEFAULT_MAX_TEMP_BYTES,
    DEFAULT_MAX_TEMP_FILES, DEFAULT_SORT_MEMORY_BUDGET_BYTES, TEMP_RUN_MAGIC, TEMP_RUN_VERSION,
};

pub use feedback::{
    compute_estimation_error, FeedbackAction, FeedbackContext, FeedbackEntry, FeedbackKey,
    FeedbackObservation, FeedbackRegistry, ResourceFeedback, ResourceObservation, SpillStatus,
    DEFAULT_FEEDBACK_ALPHA, DEFAULT_FEEDBACK_CAPACITY, DEFAULT_REPLAN_MIN_OBSERVATIONS,
    DEFAULT_REPLAN_THRESHOLD_RATIO, OUTLIER_MAX_RATIO, OUTLIER_MIN_RATIO,
};
pub use fingerprint::{
    canonicalize_plan, fnv1a_128, fnv1a_64, PhysicalPlanFingerprint,
    PHYSICAL_PLAN_FINGERPRINT_VERSION,
};
pub use lifecycle::{
    AdaptivePlanSelector, AdaptiveSelectionConfig, AdaptiveSelectionDiagnostics,
    AdaptiveSelectionMetrics, AdaptiveSelectionResult, FeedbackConfidence, PlanCandidate,
    PlanCandidateSet, PlanLifecycleState, PlanObservation, ReplanDecision, DEFAULT_MAX_CANDIDATES,
    DEFAULT_MAX_COST_ADJUSTMENT_RATIO, DEFAULT_MIN_CONFIDENCE, DEFAULT_MIN_OBSERVATIONS,
    DEFAULT_SWITCH_THRESHOLD,
};
pub use materialize::{
    estimate_row_memory as estimate_materialize_row_memory, LogicalSubplanFingerprint,
    MaterializationDecision, MaterializationMetrics, MaterializeConfig, MaterializeOperator,
    MaterializeOverride, MaterializedStorage, StatementMaterializeStore, SubplanFingerprint,
    TempMaterializeFile, DEFAULT_MATERIALIZE_MEMORY_BUDGET_BYTES,
    DEFAULT_MAX_MATERIALIZE_TEMP_BYTES, TEMP_MATERIALIZE_MAGIC, TEMP_MATERIALIZE_VERSION,
};
pub use metrics::{
    estimate_error_ratio, find_operator_metrics, format_plan_with_metrics, OperatorCounters,
    OperatorKind, OperatorMetrics, QueryExecutionMetrics,
};
pub use operator::{
    compare_evaluated_values, compare_rows_by_sort_keys, compare_rows_prepared,
    compare_values_for_sort, AggregateOperator, AggregateState, CanonicalValue, FilterOperator,
    GroupKey, JoinOperator, OffsetLimitOperator, PreparedSortKey, ProjectOperator, QueryResult,
    SetOperationOperator, SortDirection, SortKey, SortOperator, WindowOperator,
};
pub use optimizer::{
    recombine_disjunction, split_disjunction, AccessPath, JoinStrategy, MultiIndexChildScan,
    MultiIndexStrategy, Optimizer, PhysicalJoin, PhysicalPlan, PhysicalSetOp, PhysicalTableScan,
    SecondaryScanType, TableOptInfo, MAX_MULTI_INDEXES,
};
pub use plan_cache::{
    extract_plan_references, ParamPlanningSignature, PlanCache, PlanCacheEntry, PlanCacheKey,
    PlanCacheStats, PlanValidity, ResourcePlanningSignature, DEFAULT_PLAN_CACHE_CAPACITY,
};
pub use reference::{
    assert_differential_equivalence, CanonicalRow, DifferentialFailureReport, ReferenceEvaluator,
};
pub use rewrite::{QueryRewriter, RewriteResult};
pub use stats::{
    ColumnStatistics, IndexStatistics, StatisticsProvider, StatsRegistry, TableStatistics,
};
pub use validation::{
    PhysicalPlanValidator, PlanValidationContext, PlanValidationError, TableValidationInfo,
    ValidatedPhysicalPlan,
};
