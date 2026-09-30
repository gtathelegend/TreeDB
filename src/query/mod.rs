pub mod builder;
pub mod cardinality;
pub mod cost;
pub mod expr;
pub mod external_aggregate;
pub mod external_hash_join;
pub mod external_sort;
pub mod metrics;
pub mod operator;
pub mod optimizer;
pub mod plan_cache;
pub mod rewrite;
pub mod stats;

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
pub use external_sort::{
    estimate_row_memory, write_sorted_run, ExternalSorter, RunReader, SortConfig,
    SortExecutionMetrics, TempRunFile, DEFAULT_MAX_MERGE_PASSES, DEFAULT_MAX_TEMP_BYTES,
    DEFAULT_MAX_TEMP_FILES, DEFAULT_SORT_MEMORY_BUDGET_BYTES, TEMP_RUN_MAGIC, TEMP_RUN_VERSION,
};
pub use metrics::{
    estimate_error_ratio, find_operator_metrics, format_plan_with_metrics, OperatorCounters,
    OperatorKind, OperatorMetrics, QueryExecutionMetrics,
};
pub use operator::{
    compare_rows_by_sort_keys, compare_values_for_sort, AggregateOperator, AggregateState,
    CanonicalValue, FilterOperator, GroupKey, JoinOperator, OffsetLimitOperator, ProjectOperator,
    QueryResult, SetOperationOperator, SortDirection, SortKey, SortOperator, WindowOperator,
};
pub use optimizer::{
    recombine_disjunction, split_disjunction, AccessPath, JoinStrategy, MultiIndexChildScan,
    MultiIndexStrategy, Optimizer, PhysicalJoin, PhysicalPlan, PhysicalSetOp, PhysicalTableScan,
    SecondaryScanType, TableOptInfo, MAX_MULTI_INDEXES,
};
pub use plan_cache::{
    extract_plan_references, ParamPlanningSignature, PlanCache, PlanCacheEntry, PlanCacheKey,
    PlanCacheStats, PlanValidity, DEFAULT_PLAN_CACHE_CAPACITY,
};
pub use rewrite::{QueryRewriter, RewriteResult};
pub use stats::{
    ColumnStatistics, IndexStatistics, StatisticsProvider, StatsRegistry, TableStatistics,
};
