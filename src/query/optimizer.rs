use std::collections::HashSet;
use std::fmt;

use crate::engine::catalog::{IndexId, IndexMetadata};
use crate::engine::types::{DataType, Schema, Value};
use crate::error::Result;
use crate::query::cardinality::CardinalityEstimator;
use crate::query::cost::{CostModel, SEC_INDEX_FETCH_COST, SEC_INDEX_LOOKUP_COST};
use crate::query::expr::{AggregateExpr, BinaryOp, Expr};
use crate::query::external_aggregate::DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES;
use crate::query::external_hash_join::DEFAULT_JOIN_MEMORY_BUDGET_BYTES;
use crate::query::external_multi_index::{
    MultiIndexExecutionMode, DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES, RECORD_ID_IN_MEMORY_BYTES,
};
use crate::query::external_sort::DEFAULT_SORT_MEMORY_BUDGET_BYTES;
use crate::query::feedback::{FeedbackContext, FeedbackKey, FeedbackRegistry, SpillStatus};
use crate::query::inference::{
    ColumnConstraint, ConstraintSet, InferenceMetrics, InferredPredicate,
};
use crate::query::operator::SortKey;
use crate::query::plan_cache::{ParamPlanningSignature, ResourcePlanningSignature};
use crate::query::stats::TableStatistics;
use crate::sql::ast::JoinType;
use crate::sql::binder::BoundSelect;

/// Strategy for combining candidate RecordId sets from multiple index scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiIndexStrategy {
    /// Candidate sets are intersected (AND conjunction).
    Intersection,
    /// Candidate sets are unioned with deduplication (OR disjunction).
    Union,
}

impl fmt::Display for MultiIndexStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Intersection => write!(f, "INTERSECTION"),
            Self::Union => write!(f, "UNION"),
        }
    }
}

/// An individual index scan contributing candidate RecordIds to a multi-index scan.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiIndexChildScan {
    /// Name of the participating secondary index.
    pub index_name: String,
    /// Identifier of the participating secondary index.
    pub index_id: IndexId,
    /// Secondary scan mode.
    pub scan_type: SecondaryScanType,
    /// Estimated candidate row count produced by this index scan.
    pub estimated_rows: f64,
    /// Estimated candidate scan cost for this index scan.
    pub cost: f64,
}

impl fmt::Display for MultiIndexChildScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.index_name, self.scan_type)
    }
}

/// Bounded limit on the number of secondary indexes combined in a multi-index scan.
pub const MAX_MULTI_INDEXES: usize = 3;

/// Physical access path for retrieving candidate rows from an individual table relation.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessPath {
    /// Exact point lookup using the primary key B+ Tree index.
    PrimaryPointLookup { key: Value },
    /// Ordered range scan on the primary key in `[start, end)`.
    PrimaryRangeScan {
        start: Option<Value>,
        end: Option<Value>,
    },
    /// Secondary index lookup or range scan.
    SecondaryIndexScan {
        index_name: String,
        index_id: IndexId,
        scan_type: SecondaryScanType,
    },
    /// Index-only scan satisfying all query-referenced columns without fetching base records from RecordStore.
    IndexOnlyScan {
        index_name: String,
        index_id: IndexId,
        scan_type: SecondaryScanType,
        covered_columns: Vec<String>,
    },
    /// Multi-index candidate generation combining multiple secondary indexes via intersection or union.
    MultiIndexScan {
        strategy: MultiIndexStrategy,
        child_scans: Vec<MultiIndexChildScan>,
        execution_mode: MultiIndexExecutionMode,
    },
    /// Full sequential scan over all tuples.
    FullTableScan,
    /// Direct zero-row result for proven contradictory / unsatisfiable constraints.
    EmptyResult,
}

impl fmt::Display for AccessPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrimaryPointLookup { key } => {
                write!(f, "PrimaryPointLookup(key={key})")
            }
            Self::PrimaryRangeScan { start, end } => {
                write!(
                    f,
                    "PrimaryRangeScan(start={}, end={})",
                    start
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "None".into()),
                    end.as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "None".into())
                )
            }
            Self::SecondaryIndexScan {
                index_name,
                scan_type,
                ..
            } => match scan_type {
                SecondaryScanType::PointLookup(val) => {
                    write!(f, "SecondaryIndexScan({index_name}, PointLookup({val}))")
                }
                SecondaryScanType::CompositePointLookup(vals) => {
                    let tuple_str = vals
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(
                        f,
                        "SecondaryIndexScan({index_name}, PointLookup(({tuple_str})))"
                    )
                }
                SecondaryScanType::PrefixScan(vals) => {
                    let tuple_str = vals
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(
                        f,
                        "SecondaryIndexScan({index_name}, PrefixScan(({tuple_str})))"
                    )
                }
                SecondaryScanType::RangeScan { start, end } => {
                    write!(
                        f,
                        "SecondaryIndexScan({index_name}, RangeScan(start={}, end={}))",
                        start
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "None".into()),
                        end.as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "None".into())
                    )
                }
                SecondaryScanType::SkipScan {
                    skipped_prefix_columns,
                    constrained_columns,
                } => {
                    let prefix_str = skipped_prefix_columns.join(", ");
                    let constr_str = constrained_columns
                        .iter()
                        .map(|(col, val)| format!("{col} = {val}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(
                        f,
                        "IndexSkipScan(index={index_name}, skipped_prefix=[{prefix_str}], constrained=[{constr_str}])"
                    )
                }
            },
            Self::IndexOnlyScan {
                index_name,
                scan_type,
                covered_columns,
                ..
            } => {
                let cov_str = covered_columns.join(", ");
                match scan_type {
                    SecondaryScanType::PointLookup(val) => {
                        write!(
                            f,
                            "IndexOnlyScan({index_name}, PointLookup({val}), covered=[{cov_str}])"
                        )
                    }
                    SecondaryScanType::CompositePointLookup(vals) => {
                        let tuple_str = vals
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(
                            f,
                            "IndexOnlyScan({index_name}, PointLookup(({tuple_str})), covered=[{cov_str}])"
                        )
                    }
                    SecondaryScanType::PrefixScan(vals) => {
                        let tuple_str = vals
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(
                            f,
                            "IndexOnlyScan({index_name}, PrefixScan(({tuple_str})), covered=[{cov_str}])"
                        )
                    }
                    SecondaryScanType::RangeScan { start, end } => {
                        write!(
                            f,
                            "IndexOnlyScan({index_name}, RangeScan(start={}, end={}), covered=[{cov_str}])",
                            start
                                .as_ref()
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "None".into()),
                            end.as_ref()
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "None".into())
                        )
                    }
                    SecondaryScanType::SkipScan {
                        skipped_prefix_columns,
                        constrained_columns,
                    } => {
                        let prefix_str = skipped_prefix_columns.join(", ");
                        let constr_str = constrained_columns
                            .iter()
                            .map(|(col, val)| format!("{col} = {val}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(
                            f,
                            "IndexOnlySkipScan(index={index_name}, skipped_prefix=[{prefix_str}], constrained=[{constr_str}], covered=[{cov_str}])"
                        )
                    }
                }
            }
            Self::MultiIndexScan {
                strategy,
                child_scans,
                execution_mode,
            } => {
                let scans_str = child_scans
                    .iter()
                    .map(|cs| format!("{}({})", cs.index_name, cs.scan_type))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "MultiIndexScan(strategy={strategy}, execution={execution_mode}, indexes=[{scans_str}])"
                )
            }
            Self::FullTableScan => write!(f, "FullTableScan"),
            Self::EmptyResult => write!(f, "EmptyResult"),
        }
    }
}

/// Secondary index scan mode.
#[derive(Debug, Clone, PartialEq)]
pub enum SecondaryScanType {
    /// Exact point lookup on a single-column indexed value.
    PointLookup(Value),
    /// Exact point lookup on all columns of a composite secondary index.
    CompositePointLookup(Vec<Value>),
    /// Prefix scan on leading columns of a composite secondary index.
    PrefixScan(Vec<Value>),
    /// Range scan on an indexed column in `[start, end)`.
    RangeScan {
        start: Option<Value>,
        end: Option<Value>,
    },
    /// Skip scan on a composite index with skipped leading prefix and constrained suffix.
    SkipScan {
        skipped_prefix_columns: Vec<String>,
        constrained_columns: Vec<(String, Value)>,
    },
}

impl fmt::Display for SecondaryScanType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PointLookup(val) => write!(f, "PointLookup({val})"),
            Self::CompositePointLookup(vals) => {
                let s = vals
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "PointLookup(({s}))")
            }
            Self::PrefixScan(vals) => {
                let s = vals
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "PrefixScan(({s}))")
            }
            Self::RangeScan { start, end } => {
                write!(
                    f,
                    "RangeScan(start={}, end={})",
                    start
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "None".into()),
                    end.as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "None".into())
                )
            }
            Self::SkipScan {
                skipped_prefix_columns,
                constrained_columns,
            } => {
                let p = skipped_prefix_columns.join(", ");
                let c = constrained_columns
                    .iter()
                    .map(|(col, val)| format!("{col} = {val}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "SkipScan(skipped=[{p}], constrained=[{c}])")
            }
        }
    }
}

/// Physical table scan node.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalTableScan {
    /// Canonical table name.
    pub table_name: String,
    /// Optional table alias.
    pub table_alias: Option<String>,
    /// Selected physical access path.
    pub access_path: AccessPath,
    /// Table-local residual filtering predicate to be evaluated on candidate rows.
    pub residual_filter: Option<Expr>,
    /// Estimated output row count from this table scan.
    pub estimated_rows: f64,
    /// Estimated cost of this table scan.
    pub cost: f64,
    /// Bounded row goal requirement if active for this scan node.
    pub row_goal: Option<crate::query::physical_properties::RowGoal>,
    /// Estimated candidate entries/rows examined under row goal.
    pub estimated_rows_examined: Option<f64>,
    /// Estimated qualifying rows output under row goal.
    pub estimated_qualifying_rows: Option<f64>,
    /// Whether early termination is planned for this scan node.
    pub early_termination_possible: bool,
    /// Required column names needed from this scan by the consumer execution tree.
    pub required_columns: Vec<String>,
    /// Pruned unreferenced column names omitted from demand.
    pub pruned_columns: Vec<String>,
    /// Covered column names if scan utilizes covering index properties.
    pub covered_columns: Vec<String>,
    /// Whether late base-record materialization is enabled for this scan node.
    pub late_materialization: bool,
    /// Index-covered filtering predicate evaluated before base record fetching.
    pub index_covered_filter: Option<Expr>,
    /// Base-record filtering predicate evaluated after base record fetching.
    pub base_record_filter: Option<Expr>,
}

impl PhysicalTableScan {
    /// Returns the active qualifier (alias if present, else table name).
    pub fn qualifier(&self) -> &str {
        self.table_alias.as_deref().unwrap_or(&self.table_name)
    }

    /// Creates an empty result physical table scan.
    pub fn new_empty(table_name: String, table_alias: Option<String>) -> Self {
        Self::new_with_access_path(table_name, table_alias, AccessPath::EmptyResult)
    }

    /// Creates a physical table scan with a specified access path.
    pub fn new_with_access_path(
        table_name: String,
        table_alias: Option<String>,
        access_path: AccessPath,
    ) -> Self {
        Self {
            table_name,
            table_alias,
            access_path,
            residual_filter: None,
            estimated_rows: 0.0,
            cost: 0.0,
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            early_termination_possible: false,
            required_columns: Vec::new(),
            pruned_columns: Vec::new(),
            covered_columns: Vec::new(),
            late_materialization: false,
            index_covered_filter: None,
            base_record_filter: None,
        }
    }
}

/// Execution strategy chosen for a physical relational join operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinStrategy {
    /// Nested-loop join evaluating on-predicate across Cartesian product or with early match exit.
    NestedLoop,
    /// In-memory or streaming hash join.
    Hash,
    /// Existence probe against an inner index (primary key or secondary index).
    IndexProbe,
    /// Cost-based merge join over ordered or sorted inputs.
    Merge,
}

impl fmt::Display for JoinStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedLoop => write!(f, "NestedLoop"),
            Self::Hash => write!(f, "Hash"),
            Self::IndexProbe => write!(f, "IndexProbe"),
            Self::Merge => write!(f, "Merge"),
        }
    }
}

/// Physical relational join node.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalJoin {
    /// Relational join type (Inner, Left, Semi, Anti, etc.).
    pub join_type: JoinType,
    /// Right-hand table scan node.
    pub right: PhysicalTableScan,
    /// Join condition boolean predicate.
    pub on: Expr,
    /// Estimated output row count after this join step.
    pub estimated_rows: f64,
    /// Estimated total cumulative cost through this join step.
    pub cost: f64,
    /// Physical execution strategy.
    pub strategy: JoinStrategy,
    /// Optional index name utilized when strategy is `IndexProbe`.
    pub index_name: Option<String>,
    /// Optional runtime join filter planned for dynamic probe filtering.
    pub runtime_filter: Option<crate::query::runtime_filter::RuntimeFilterPlan>,
    /// Whether the left input needs explicit sorting for Merge Join.
    pub left_sort: bool,
    /// Whether the right input needs explicit sorting for Merge Join.
    pub right_sort: bool,
    /// Optional output row goal requested by parent query.
    pub row_goal: Option<crate::query::physical_properties::RowGoal>,
    /// Estimated rows/entries examined to satisfy the row goal.
    pub estimated_rows_examined: Option<f64>,
    /// Estimated qualifying rows passing join predicates.
    pub estimated_qualifying_rows: Option<f64>,
    /// Whether early termination is semantically safe and possible.
    pub early_termination_possible: bool,
    /// Which execution phase can terminate early (e.g. "possible", "probe phase").
    pub early_stop_phase: Option<String>,
}

/// Physical relational set operation node.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalSetOp {
    /// Set operator kind.
    pub op: crate::sql::SetOperator,
    /// Left input physical plan.
    pub left: Box<PhysicalPlan>,
    /// Right input physical plan.
    pub right: Box<PhysicalPlan>,
    /// Target unified schema for the set operation.
    pub schema: Schema,
}

/// Execution strategy chosen for a physical aggregation operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateStrategy {
    /// In-memory hash aggregation.
    Hash,
    /// Ordered streaming aggregation over ordered or sorted input stream.
    Streaming { sort_required: bool },
    /// Spilled external hash aggregation.
    External,
}

impl fmt::Display for AggregateStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash => write!(f, "HashAggregate"),
            Self::Streaming { sort_required } => write!(
                f,
                "StreamingAggregate(sort_required: {})",
                if *sort_required { "Yes" } else { "No" }
            ),
            Self::External => write!(f, "ExternalAggregate"),
        }
    }
}

/// Complete physical execution plan produced by the cost-based optimizer.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPlan {
    /// Statement-local CTE definitions.
    pub ctes: Vec<crate::sql::binder::BoundCte>,
    /// Base relation scan node (FROM table).
    pub base_table: PhysicalTableScan,
    /// Physical joins in evaluation order.
    pub joins: Vec<PhysicalJoin>,
    /// Global post-join filter predicate (WHERE condition across multiple tables).
    pub post_join_filter: Option<Expr>,
    /// Grouping expressions (GROUP BY).
    pub group_by: Vec<Expr>,
    /// Aggregate expressions.
    pub aggregates: Vec<AggregateExpr>,
    /// Physical execution strategy for aggregation.
    pub aggregate_strategy: Option<AggregateStrategy>,
    /// Post-aggregation filter predicate (HAVING).
    pub having: Option<Expr>,
    /// Window expressions to compute.
    pub window_exprs: Vec<crate::query::expr::WindowExpr>,
    /// Output projection expressions with optional aliases.
    pub projections: Vec<(Expr, Option<String>)>,
    /// Sort keys (ORDER BY).
    pub order_by: Vec<SortKey>,
    /// Whether the ORDER BY clause requires an explicit Sort execution operator.
    pub order_by_sort_required: bool,
    /// Whether the ORDER BY clause uses bounded Top-N heap execution.
    pub order_by_top_n: bool,
    /// Active physical ordering property guaranteed at plan root.
    pub provided_ordering: Option<crate::query::physical_properties::OrderingProperty>,
    /// Offset pagination.
    pub offset: Option<usize>,
    /// Limit pagination.
    pub limit: Option<usize>,
    /// Optional physical set operation.
    pub set_op: Option<PhysicalSetOp>,
    /// Logical transformations applied during the query rewrite phase.
    pub logical_rewrites: Vec<String>,
    /// Inferred predicates and constraint diagnostics.
    pub inferred_predicates: Vec<InferredPredicate>,
    /// Bounded row goal requirement if active for plan root.
    pub row_goal: Option<crate::query::physical_properties::RowGoal>,
    /// Estimated candidate entries/rows examined under row goal.
    pub estimated_rows_examined: Option<f64>,
    /// Estimated qualifying rows output under row goal.
    pub estimated_qualifying_rows: Option<f64>,
    /// Whether early termination is planned under row goal.
    pub row_goal_early_termination: bool,
    /// Total estimated execution cost of this physical plan.
    pub total_cost: f64,
    /// Total estimated output row count of this physical plan.
    pub estimated_rows: f64,
}

impl PhysicalPlan {
    /// Creates a new physical plan for a relational set operation.
    pub fn new_set_op(set_op: PhysicalSetOp) -> Self {
        let left_cost = set_op.left.total_cost;
        let right_cost = set_op.right.total_cost;
        let total_cost = left_cost + right_cost;
        let estimated_rows = set_op.left.estimated_rows + set_op.right.estimated_rows;

        Self {
            ctes: Vec::new(),
            base_table: PhysicalTableScan {
                table_name: String::new(),
                table_alias: None,
                access_path: AccessPath::FullTableScan,
                residual_filter: None,
                estimated_rows: 0.0,
                cost: 0.0,
                row_goal: None,
                estimated_rows_examined: None,
                estimated_qualifying_rows: None,
                early_termination_possible: false,
                required_columns: Vec::new(),
                pruned_columns: Vec::new(),
                covered_columns: Vec::new(),
                late_materialization: false,
                index_covered_filter: None,
                base_record_filter: None,
            },
            joins: Vec::new(),
            post_join_filter: None,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            aggregate_strategy: None,
            having: None,
            window_exprs: Vec::new(),
            projections: Vec::new(),
            order_by: Vec::new(),
            order_by_sort_required: false,
            order_by_top_n: false,
            provided_ordering: None,
            offset: None,
            limit: None,
            set_op: Some(set_op),
            logical_rewrites: Vec::new(),
            inferred_predicates: Vec::new(),
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            row_goal_early_termination: false,
            total_cost,
            estimated_rows,
        }
    }
}

impl fmt::Display for PhysicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut indent = 0;
        let write_indent = |f: &mut fmt::Formatter<'_>, lvl: usize| -> fmt::Result {
            for _ in 0..lvl {
                write!(f, "  ")?;
            }
            Ok(())
        };

        if !self.logical_rewrites.is_empty() {
            write_indent(f, indent)?;
            writeln!(f, "Logical Rewrite")?;
            for rw in &self.logical_rewrites {
                write_indent(f, indent + 1)?;
                writeln!(f, "{rw}")?;
            }
        }

        if !self.inferred_predicates.is_empty() {
            write_indent(f, indent)?;
            writeln!(f, "Predicate Inference")?;
            for inf in &self.inferred_predicates {
                write_indent(f, indent + 1)?;
                writeln!(f, "original:\n    {}", inf.original)?;
                write_indent(f, indent + 1)?;
                writeln!(f, "inferred:\n    {}", inf.inferred)?;
                write_indent(f, indent + 1)?;
                writeln!(f, "source:\n    {}", inf.source)?;
            }
        }

        if matches!(self.base_table.access_path, AccessPath::EmptyResult) {
            write_indent(f, indent)?;
            writeln!(f, "Predicate Inference")?;
            write_indent(f, indent + 1)?;
            writeln!(f, "contradiction:\n    unsatisfiable constraint")?;
            write_indent(f, indent + 1)?;
            writeln!(f, "result:\n    EmptyResult")?;
        }

        if !self.ctes.is_empty() {
            for cte in &self.ctes {
                write_indent(f, indent)?;
                match &cte.kind {
                    crate::sql::binder::BoundCteKind::NonRecursive {
                        plan,
                        decision,
                        materialize_id: _,
                        consumer_count,
                        estimated_inline_cost,
                        estimated_materialize_cost,
                        fingerprint: _,
                    } => {
                        writeln!(f, "CTE {}", cte.name)?;
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Consumers: {consumer_count}")?;
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Estimated Rows: {}", plan.estimated_rows)?;
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Decision: {decision}")?;
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Estimated Inline Cost: {estimated_inline_cost:.2}")?;
                        write_indent(f, indent + 1)?;
                        writeln!(
                            f,
                            "Estimated Materialize Cost: {estimated_materialize_cost:.2}"
                        )?;
                        if !cte.dependencies.is_empty() {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "depends on: [{}]", cte.dependencies.join(", "))?;
                        }
                        let plan_str = format!("{}", plan);
                        for line in plan_str.lines() {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "{line}")?;
                        }
                    }
                    crate::sql::binder::BoundCteKind::Recursive {
                        anchor_plan,
                        recursive_plan,
                        max_iterations,
                        max_rows,
                    } => {
                        writeln!(f, "Recursive CTE {}", cte.name)?;
                        if !cte.dependencies.is_empty() {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "depends on: [{}]", cte.dependencies.join(", "))?;
                        }
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Anchor")?;
                        let anchor_str = format!("{}", anchor_plan);
                        for line in anchor_str.lines() {
                            write_indent(f, indent + 2)?;
                            writeln!(f, "{line}")?;
                        }
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Recursive Member")?;
                        let rec_str = format!("{}", recursive_plan);
                        for line in rec_str.lines() {
                            write_indent(f, indent + 2)?;
                            writeln!(f, "{line}")?;
                        }
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Max Iterations: {max_iterations}")?;
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Max Rows: {max_rows}")?;
                    }
                }
            }
            write_indent(f, indent)?;
            writeln!(f, "Main Query")?;
            indent += 1;
        }

        if (self.limit.is_some() || self.offset.is_some()) && !self.order_by_top_n {
            write_indent(f, indent)?;
            writeln!(
                f,
                "LimitOffset (limit: {:?}, offset: {:?})",
                self.limit, self.offset
            )?;
            indent += 1;
        }

        if !self.projections.is_empty() {
            write_indent(f, indent)?;
            let projs: Vec<String> = self
                .projections
                .iter()
                .map(|(e, a)| match a {
                    Some(alias) => format!("{e} AS {alias}"),
                    None => format!("{e}"),
                })
                .collect();
            writeln!(f, "Projection [{}]", projs.join(", "))?;
            indent += 1;
        }

        if !self.order_by.is_empty() && self.order_by_sort_required {
            write_indent(f, indent)?;
            let keys: Vec<String> = self
                .order_by
                .iter()
                .map(|k| format!("{} {:?}", k.expression, k.direction))
                .collect();
            if self.order_by_top_n {
                writeln!(
                    f,
                    "TopN (limit: {:?}, offset: {:?}) [{}]",
                    self.limit,
                    self.offset,
                    keys.join(", ")
                )?;
            } else {
                writeln!(f, "Sort [{}]", keys.join(", "))?;
            }
            indent += 1;
        }

        if let Some(set_op) = &self.set_op {
            write_indent(f, indent)?;
            writeln!(f, "Set Operation: {}", set_op.op)?;
            write_indent(f, indent + 1)?;
            writeln!(f, "Left")?;
            let left_str = format!("{}", set_op.left);
            for line in left_str.lines() {
                write_indent(f, indent + 2)?;
                writeln!(f, "{line}")?;
            }
            write_indent(f, indent + 1)?;
            writeln!(f, "Right")?;
            let right_str = format!("{}", set_op.right);
            for line in right_str.lines() {
                write_indent(f, indent + 2)?;
                writeln!(f, "{line}")?;
            }
            return Ok(());
        }

        if !self.window_exprs.is_empty() {
            write_indent(f, indent)?;
            writeln!(f, "Window")?;
            for we in &self.window_exprs {
                write_indent(f, indent + 1)?;
                writeln!(f, "Functions:")?;
                write_indent(f, indent + 2)?;
                let args_str = if we.args.is_empty() {
                    String::new()
                } else {
                    let a: Vec<String> = we.args.iter().map(|e| e.to_string()).collect();
                    a.join(", ")
                };
                writeln!(f, "{}({})", we.function, args_str)?;

                if !we.window_spec.partition_by.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Partition By:")?;
                    for p in &we.window_spec.partition_by {
                        write_indent(f, indent + 2)?;
                        writeln!(f, "{p}")?;
                    }
                }

                if !we.window_spec.order_by.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Order By:")?;
                    for o in &we.window_spec.order_by {
                        write_indent(f, indent + 2)?;
                        writeln!(f, "{} {:?}", o.expression, o.direction)?;
                    }
                }

                write_indent(f, indent + 1)?;
                writeln!(f, "Frame:")?;
                write_indent(f, indent + 2)?;
                writeln!(
                    f,
                    "{} -> {}",
                    we.window_spec.frame.start, we.window_spec.frame.end
                )?;
            }
            indent += 1;
        }

        if let Some(having) = &self.having {
            write_indent(f, indent)?;
            writeln!(f, "Having [{}]", having)?;
            indent += 1;
        }

        if !self.group_by.is_empty() || !self.aggregates.is_empty() {
            write_indent(f, indent)?;
            let groups: Vec<String> = self.group_by.iter().map(|g| g.to_string()).collect();
            let aggs: Vec<String> = self.aggregates.iter().map(|a| a.to_string()).collect();
            match self.aggregate_strategy {
                Some(AggregateStrategy::Streaming { sort_required }) => {
                    writeln!(
                        f,
                        "StreamingAggregate (group_by: [{}], aggs: [{}], sort_required: {})",
                        groups.join(", "),
                        aggs.join(", "),
                        if sort_required { "Yes" } else { "No" }
                    )?;
                }
                Some(AggregateStrategy::External) => {
                    writeln!(
                        f,
                        "ExternalAggregate (group_by: [{}], aggs: [{}])",
                        groups.join(", "),
                        aggs.join(", ")
                    )?;
                }
                _ => {
                    writeln!(
                        f,
                        "Aggregate (group_by: [{}], aggs: [{}])",
                        groups.join(", "),
                        aggs.join(", ")
                    )?;
                }
            }
            indent += 1;
        }

        if let Some(post_filter) = &self.post_join_filter {
            write_indent(f, indent)?;
            writeln!(f, "Filter [{}]", post_filter)?;
            format_subqueries_in_expr(post_filter, f, indent + 1)?;
            indent += 1;
        }

        if self.joins.is_empty() {
            write_indent(f, indent)?;
            writeln!(
                f,
                "TableScan (table: {}{}, access: {}{})",
                self.base_table.table_name,
                self.base_table
                    .table_alias
                    .as_ref()
                    .map(|a| format!(" AS {a}"))
                    .unwrap_or_default(),
                self.base_table.access_path,
                self.base_table
                    .residual_filter
                    .as_ref()
                    .map(|rf| format!(", filter: [{rf}]"))
                    .unwrap_or_default()
            )?;
            write_indent(f, indent + 1)?;
            writeln!(
                f,
                "cost: {:.2}, estimated_rows: {:.0}",
                self.base_table.cost, self.base_table.estimated_rows
            )?;
            if !self.base_table.required_columns.is_empty() {
                write_indent(f, indent + 1)?;
                writeln!(
                    f,
                    "Required Columns: [{}]",
                    self.base_table.required_columns.join(", ")
                )?;
            }
            if !self.base_table.pruned_columns.is_empty() {
                write_indent(f, indent + 1)?;
                writeln!(
                    f,
                    "Pruned Columns: [{}]",
                    self.base_table.pruned_columns.join(", ")
                )?;
            }
            if !self.base_table.covered_columns.is_empty() {
                write_indent(f, indent + 1)?;
                writeln!(
                    f,
                    "Covered Columns: [{}]",
                    self.base_table.covered_columns.join(", ")
                )?;
            }
            if self.base_table.late_materialization {
                write_indent(f, indent + 1)?;
                writeln!(f, "Late Materialization: true")?;
                if let Some(icf) = &self.base_table.index_covered_filter {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Index-Covered Filter: [{icf}]")?;
                }
                if let Some(brf) = &self.base_table.base_record_filter {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Base-Record Filter: [{brf}]")?;
                }
            }
            if let Some(rg) = &self.base_table.row_goal {
                write_indent(f, indent + 1)?;
                writeln!(f, "Row Goal: {}", rg.k())?;
            }
            if let Some(examined) = self.base_table.estimated_rows_examined {
                write_indent(f, indent + 1)?;
                writeln!(f, "Estimated Rows Examined: {:.0}", examined)?;
            }
            if let Some(qualifying) = self.base_table.estimated_qualifying_rows {
                write_indent(f, indent + 1)?;
                writeln!(f, "Estimated Qualifying Rows: {:.0}", qualifying)?;
            }
            if self.base_table.early_termination_possible {
                write_indent(f, indent + 1)?;
                writeln!(f, "Early Termination: possible")?;
            }
            if let Some(rf) = &self.base_table.residual_filter {
                format_subqueries_in_expr(rf, f, indent + 1)?;
            }
        } else {
            for (idx, join) in self.joins.iter().enumerate() {
                write_indent(f, indent)?;
                if join.join_type == JoinType::Semi || join.join_type == JoinType::Anti {
                    let base_title = match (join.join_type, join.strategy) {
                        (JoinType::Semi, JoinStrategy::IndexProbe) => "SemiJoin [IndexSemiProbe]",
                        (JoinType::Semi, JoinStrategy::Hash) => "SemiJoin [HashSemiJoin]",
                        (JoinType::Semi, JoinStrategy::NestedLoop) => {
                            "SemiJoin [NestedLoopSemiJoin]"
                        }
                        (JoinType::Anti, JoinStrategy::IndexProbe) => "AntiJoin [IndexAntiProbe]",
                        (JoinType::Anti, JoinStrategy::Hash) => "AntiJoin [HashAntiJoin]",
                        (JoinType::Anti, JoinStrategy::NestedLoop) => {
                            "AntiJoin [NestedLoopAntiJoin]"
                        }
                        _ => "Join",
                    };
                    writeln!(f, "{base_title} (on: [{}])", join.on)?;
                    write_indent(f, indent + 1)?;
                    writeln!(f, "strategy={:?}", join.strategy)?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "cost: {:.2}, estimated_rows: {:.0}",
                        join.cost, join.estimated_rows
                    )?;
                    if let Some(idx_name) = &join.index_name {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "index={idx_name}")?;
                    }
                } else if join.strategy == JoinStrategy::Merge {
                    writeln!(f, "MergeJoin ({:?}, on: [{}])", join.join_type, join.on)?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "Left Sort: {}, Right Sort: {}",
                        if join.left_sort { "Yes" } else { "No" },
                        if join.right_sort { "Yes" } else { "No" }
                    )?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "cost: {:.2}, estimated_rows: {:.0}",
                        join.cost, join.estimated_rows
                    )?;
                } else if join.strategy == JoinStrategy::Hash {
                    writeln!(f, "HashJoin ({:?}, on: [{}])", join.join_type, join.on)?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "cost: {:.2}, estimated_rows: {:.0}",
                        join.cost, join.estimated_rows
                    )?;
                } else {
                    writeln!(
                        f,
                        "NestedLoopJoin ({:?}, on: [{}])",
                        join.join_type, join.on
                    )?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "cost: {:.2}, estimated_rows: {:.0}",
                        join.cost, join.estimated_rows
                    )?;
                }
                if let Some(rg) = &join.row_goal {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Row Goal: {}", rg.k())?;
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Estimated Join Rows: {:.0}", join.estimated_rows)?;
                    if join.strategy == JoinStrategy::Hash {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Estimated Build Rows: {:.0}", join.right.estimated_rows)?;
                        if let Some(examined) = join.estimated_rows_examined {
                            write_indent(f, indent + 1)?;
                            writeln!(
                                f,
                                "Estimated Probe Rows To Goal: {:.0}",
                                (examined - join.right.estimated_rows).max(1.0)
                            )?;
                        }
                    } else if join.strategy == JoinStrategy::Merge {
                        if let Some(examined) = join.estimated_rows_examined {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "Estimated Input Rows To Goal: {:.0}", examined)?;
                        }
                    } else if let Some(examined) = join.estimated_rows_examined {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Estimated Outer Probes To Goal: {:.0}", examined)?;
                    }
                    if join.early_termination_possible {
                        write_indent(f, indent + 1)?;
                        if let Some(phase) = &join.early_stop_phase {
                            writeln!(f, "Early Termination: {phase}")?;
                        } else {
                            writeln!(f, "Early Termination: possible")?;
                        }
                    }
                }
                if let Some(rf) = &join.runtime_filter {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Runtime Filter:")?;
                    write_indent(f, indent + 2)?;
                    writeln!(f, "Type: {}", rf.filter_type)?;
                    write_indent(f, indent + 2)?;
                    writeln!(f, "Scope: {}", rf.scope)?;
                    write_indent(f, indent + 2)?;
                    let keys_str = rf
                        .key_exprs
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    writeln!(f, "Key: [{keys_str}]")?;
                }
                format_subqueries_in_expr(&join.on, f, indent + 1)?;
                indent += 1;
                if idx == 0 {
                    write_indent(f, indent)?;
                    writeln!(
                        f,
                        "Left: TableScan (table: {}{}, access: {}{})",
                        self.base_table.table_name,
                        self.base_table
                            .table_alias
                            .as_ref()
                            .map(|a| format!(" AS {a}"))
                            .unwrap_or_default(),
                        self.base_table.access_path,
                        self.base_table
                            .residual_filter
                            .as_ref()
                            .map(|rf| format!(", filter: [{rf}]"))
                            .unwrap_or_default()
                    )?;
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "cost: {:.2}, estimated_rows: {:.0}",
                        self.base_table.cost, self.base_table.estimated_rows
                    )?;
                    if !self.base_table.required_columns.is_empty() {
                        write_indent(f, indent + 1)?;
                        writeln!(
                            f,
                            "Required Columns: [{}]",
                            self.base_table.required_columns.join(", ")
                        )?;
                    }
                    if !self.base_table.pruned_columns.is_empty() {
                        write_indent(f, indent + 1)?;
                        writeln!(
                            f,
                            "Pruned Columns: [{}]",
                            self.base_table.pruned_columns.join(", ")
                        )?;
                    }
                    if !self.base_table.covered_columns.is_empty() {
                        write_indent(f, indent + 1)?;
                        writeln!(
                            f,
                            "Covered Columns: [{}]",
                            self.base_table.covered_columns.join(", ")
                        )?;
                    }
                    if self.base_table.late_materialization {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Late Materialization: true")?;
                        if let Some(icf) = &self.base_table.index_covered_filter {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "Index-Covered Filter: [{icf}]")?;
                        }
                        if let Some(brf) = &self.base_table.base_record_filter {
                            write_indent(f, indent + 1)?;
                            writeln!(f, "Base-Record Filter: [{brf}]")?;
                        }
                    }
                    if let Some(rf) = &self.base_table.residual_filter {
                        format_subqueries_in_expr(rf, f, indent + 1)?;
                    }
                }
                write_indent(f, indent)?;
                writeln!(
                    f,
                    "Right: TableScan (table: {}{}, access: {}{})",
                    join.right.table_name,
                    join.right
                        .table_alias
                        .as_ref()
                        .map(|a| format!(" AS {a}"))
                        .unwrap_or_default(),
                    join.right.access_path,
                    join.right
                        .residual_filter
                        .as_ref()
                        .map(|rf| format!(", filter: [{rf}]"))
                        .unwrap_or_default()
                )?;
                write_indent(f, indent + 1)?;
                writeln!(
                    f,
                    "cost: {:.2}, estimated_rows: {:.0}",
                    join.right.cost, join.right.estimated_rows
                )?;
                if !join.right.required_columns.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "Required Columns: [{}]",
                        join.right.required_columns.join(", ")
                    )?;
                }
                if !join.right.pruned_columns.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "Pruned Columns: [{}]",
                        join.right.pruned_columns.join(", ")
                    )?;
                }
                if !join.right.covered_columns.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(
                        f,
                        "Covered Columns: [{}]",
                        join.right.covered_columns.join(", ")
                    )?;
                }
                if join.right.late_materialization {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "Late Materialization: true")?;
                    if let Some(icf) = &join.right.index_covered_filter {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Index-Covered Filter: [{icf}]")?;
                    }
                    if let Some(brf) = &join.right.base_record_filter {
                        write_indent(f, indent + 1)?;
                        writeln!(f, "Base-Record Filter: [{brf}]")?;
                    }
                }
                if let Some(rf) = &join.right.residual_filter {
                    format_subqueries_in_expr(rf, f, indent + 1)?;
                }
            }
        }

        Ok(())
    }
}

fn format_subqueries_in_expr(
    expr: &Expr,
    f: &mut fmt::Formatter<'_>,
    indent: usize,
) -> fmt::Result {
    let write_indent = |f: &mut fmt::Formatter<'_>, lvl: usize| -> fmt::Result {
        for _ in 0..lvl {
            write!(f, "  ")?;
        }
        Ok(())
    };

    match expr {
        Expr::Subquery(sub) => {
            if sub.is_correlated {
                write_indent(f, indent)?;
                writeln!(f, "Correlated Subquery")?;
                write_indent(f, indent + 1)?;
                writeln!(f, "Outer references: [{}]", sub.outer_references.join(", "))?;
            } else {
                write_indent(f, indent)?;
                writeln!(f, "Subquery")?;
            }
            write_indent(f, indent + 1)?;
            let kind_str = match &sub.kind {
                crate::query::expr::SubqueryKind::Scalar { .. } => "Scalar",
                crate::query::expr::SubqueryKind::In { negated, .. } => {
                    if *negated {
                        "NOT IN"
                    } else {
                        "IN"
                    }
                }
                crate::query::expr::SubqueryKind::Exists { negated } => {
                    if *negated {
                        "NOT EXISTS"
                    } else {
                        "EXISTS"
                    }
                }
            };
            writeln!(f, "Kind: {kind_str}")?;
            write_indent(f, indent + 1)?;
            writeln!(f, "Plan:")?;
            let plan_str = format!("{}", sub.plan);
            for line in plan_str.lines() {
                write_indent(f, indent + 2)?;
                writeln!(f, "{line}")?;
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            format_subqueries_in_expr(left, f, indent)?;
            format_subqueries_in_expr(right, f, indent)?;
            Ok(())
        }
        Expr::Unary { expr, .. } | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            format_subqueries_in_expr(expr, f, indent)
        }
        Expr::InList { expr, list, .. } => {
            format_subqueries_in_expr(expr, f, indent)?;
            for item in list {
                format_subqueries_in_expr(item, f, indent)?;
            }
            Ok(())
        }
        Expr::Literal(_) | Expr::Parameter { .. } | Expr::Column(_) | Expr::OuterColumn { .. } => {
            Ok(())
        }
    }
}

/// Metadata information about a participating table relation passed to the optimizer.
#[derive(Debug, Clone)]
pub struct TableOptInfo {
    /// Canonical table name.
    pub name: String,
    /// Optional table alias.
    pub alias: Option<String>,
    /// Table schema.
    pub schema: Schema,
    /// Attached secondary indexes.
    pub indexes: Vec<IndexMetadata>,
    /// Whether this table represents an in-memory CTE or derived relation without persistent indexes.
    pub is_cte: bool,
    /// Advisory statistics for this table if available.
    pub stats: Option<TableStatistics>,
}

impl TableOptInfo {
    /// Creates a new `TableOptInfo`.
    pub fn new(
        name: impl Into<String>,
        alias: Option<impl Into<String>>,
        schema: Schema,
        indexes: Vec<IndexMetadata>,
    ) -> Self {
        Self {
            name: name.into(),
            alias: alias.map(|a| a.into()),
            schema,
            indexes,
            is_cte: false,
            stats: None,
        }
    }

    /// Sets whether this table is a CTE.
    pub fn with_cte(mut self, is_cte: bool) -> Self {
        self.is_cte = is_cte;
        self
    }

    /// Attaches optional table statistics.
    pub fn with_stats(mut self, stats: Option<TableStatistics>) -> Self {
        self.stats = stats;
        self
    }

    /// Returns the active qualifier (alias if present, otherwise table name).
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// Maximum number of relations enumerated via subset dynamic programming.
/// If an inner-join region contains more than this limit, a deterministic greedy search is used.
pub const MAX_ENUMERATED_RELATIONS: usize = 8;

/// A relation node in the join graph.
#[derive(Debug, Clone)]
pub struct JoinGraphRelation {
    /// Local relation ID within the join region (0..M).
    pub id: usize,
    /// Global index within the query's `tables` slice.
    pub global_idx: usize,
    /// Active table qualifier.
    pub qualifier: String,
    /// Metadata information about the table.
    pub table_info: TableOptInfo,
    /// Pre-planned base table scan.
    pub base_scan: PhysicalTableScan,
}

/// A join predicate or multi-table filter conjunct in the join graph.
#[derive(Debug, Clone)]
pub struct JoinGraphPredicate {
    /// Boolean expression of the predicate.
    pub expr: Expr,
    /// Set of table qualifiers referenced by this expression.
    pub referenced_qualifiers: HashSet<String>,
    /// Set of local relation IDs within the join region required to evaluate this predicate.
    pub referenced_rel_ids: HashSet<usize>,
    /// Whether this predicate represents a direct equi-join equality between two relations.
    pub is_equality: bool,
}

/// Relational join graph representation for an inner-join region.
#[derive(Debug, Clone)]
pub struct JoinGraph {
    /// Participating relations in this join region.
    pub relations: Vec<JoinGraphRelation>,
    /// Extracted join and filter predicates applicable to this join region.
    pub predicates: Vec<JoinGraphPredicate>,
}

/// Intermediate plan candidate used in dynamic programming enumeration and greedy fallback.
#[derive(Debug, Clone)]
pub struct DPPlan {
    /// Ordered permutation of local relation IDs: `[base_relation, join_1, join_2, ...]`.
    pub perm: Vec<usize>,
    /// Cumulative estimated cost.
    pub cost: f64,
    /// Cumulative estimated output row count.
    pub rows: f64,
    /// Physical base table scan for the chosen leading relation.
    pub base_scan: PhysicalTableScan,
    /// Sequence of physical join operations.
    pub joins: Vec<PhysicalJoin>,
    /// Bitset marking which join graph predicates have been attached.
    pub used_predicates: Vec<bool>,
    /// Number of Cartesian join steps (joins without direct connecting predicates).
    pub cartesian_joins: usize,
}

/// Deterministic comparator for selecting the lowest-cost plan candidate.
fn is_better_plan(cand: &DPPlan, existing: &DPPlan) -> bool {
    // 1. Prefer fewer Cartesian products (favor direct connected joins)
    if cand.cartesian_joins != existing.cartesian_joins {
        return cand.cartesian_joins < existing.cartesian_joins;
    }

    const EPSILON: f64 = 1e-9;
    let cost_diff = cand.cost - existing.cost;
    if cost_diff < -EPSILON {
        return true;
    }
    if cost_diff > EPSILON {
        return false;
    }

    // 2. Deterministic tie-breaker: lower estimated output cardinality
    let row_diff = cand.rows - existing.rows;
    if row_diff < -EPSILON {
        return true;
    }
    if row_diff > EPSILON {
        return false;
    }

    // 3. Deterministic tie-breaker: prefer identity permutation (original SQL relation order)
    let cand_is_identity = cand.perm.iter().enumerate().all(|(i, &p)| i == p);
    let exist_is_identity = existing.perm.iter().enumerate().all(|(i, &p)| i == p);
    if cand_is_identity && !exist_is_identity {
        return true;
    }
    if !cand_is_identity && exist_is_identity {
        return false;
    }

    // 4. Deterministic tie-breaker: lexicographically smaller permutation order
    cand.perm < existing.perm
}

/// Optimizes an inner-join region using dynamic programming over subsets ($O(n \cdot 2^n)$).
fn optimize_inner_join_region_dp(
    graph: &JoinGraph,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
    row_goal: Option<&crate::query::physical_properties::RowGoal>,
    required_ordering: Option<&crate::query::physical_properties::RequiredOrdering>,
) -> DPPlan {
    let num_rels = graph.relations.len();
    assert!(num_rels > 0 && num_rels <= MAX_ENUMERATED_RELATIONS);

    let num_states = 1 << num_rels;
    let mut dp: Vec<Option<DPPlan>> = vec![None; num_states];

    // Base cases: single-table scans (subsets of size 1)
    for i in 0..num_rels {
        let base_scan = graph.relations[i].base_scan.clone();
        dp[1 << i] = Some(DPPlan {
            perm: vec![i],
            cost: base_scan.cost,
            rows: base_scan.estimated_rows,
            base_scan,
            joins: Vec::new(),
            used_predicates: vec![false; graph.predicates.len()],
            cartesian_joins: 0,
        });
    }

    // Iterate over subset sizes from 2 to num_rels
    for size in 2..=num_rels {
        for mask in 1..num_states {
            if mask.count_ones() as usize != size {
                continue;
            }

            let mut best_for_mask: Option<DPPlan> = None;

            // Try each relation `r` as the right-most (latest joined) relation in subset `mask`
            for r in 0..num_rels {
                if (mask & (1 << r)) == 0 {
                    continue;
                }

                let left_mask = mask ^ (1 << r);
                let left_plan = match &dp[left_mask] {
                    Some(p) => p,
                    None => continue,
                };

                let right_scan = &graph.relations[r].base_scan;

                // Collect relations available in `mask`
                let mut mask_rel_ids = HashSet::new();
                for i in 0..num_rels {
                    if (mask & (1 << i)) != 0 {
                        mask_rel_ids.insert(i);
                    }
                }

                // Identify predicates attached at this join step
                let mut step_conjuncts = Vec::new();
                let mut cand_used_preds = left_plan.used_predicates.clone();
                let mut has_direct_predicate = false;

                for (p_idx, pred) in graph.predicates.iter().enumerate() {
                    if !cand_used_preds[p_idx]
                        && pred.referenced_rel_ids.is_subset(&mask_rel_ids)
                        && pred.referenced_rel_ids.contains(&r)
                    {
                        step_conjuncts.push(pred.expr.clone());
                        cand_used_preds[p_idx] = true;
                        if pred
                            .referenced_rel_ids
                            .iter()
                            .any(|&id| id != r && (left_mask & (1 << id)) != 0)
                        {
                            has_direct_predicate = true;
                        }
                    }
                }

                let on_expr = recombine_conjunction(step_conjuncts)
                    .unwrap_or(Expr::Literal(Value::Boolean(true)));

                let base_idx = left_plan.perm[0];
                let mut join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
                    left_plan.rows,
                    graph.relations[base_idx].table_info.stats.as_ref(),
                    right_scan.estimated_rows,
                    graph.relations[r].table_info.stats.as_ref(),
                    &on_expr,
                    &graph.relations[r].table_info.schema,
                );

                if let Some(fb_reg) = feedback {
                    let key = FeedbackKey::join(
                        "Inner",
                        &graph.relations[base_idx].table_info.name,
                        &graph.relations[r].table_info.name,
                        on_expr.to_string(),
                        param_sig.cloned(),
                        stats_gen,
                    )
                    .with_resource_signature(resource_sig.cloned());
                    join_rows = fb_reg.get_adjusted_cardinality(&key, join_rows);
                }

                let mut left_cols = Vec::new();
                for &idx in &left_plan.perm {
                    left_cols.extend(graph.relations[idx].table_info.schema.columns.clone());
                }
                let left_accum_schema = Schema::new_projected(left_cols);

                let rf = plan_runtime_filter_for_join(
                    JoinType::Inner,
                    &on_expr,
                    &left_accum_schema,
                    &graph.relations[r].table_info.schema,
                    right_scan.estimated_rows,
                    left_plan.rows,
                    graph.relations[r].table_info.stats.as_ref(),
                    resource_sig,
                );

                let left_ord = if left_plan.perm.len() == 1 {
                    let first = left_plan.perm[0];
                    Some(crate::query::merge_join::ordering_property_for_table_scan(
                        &graph.relations[first].table_info,
                        &graph.relations[first].base_scan,
                    ))
                } else {
                    None
                };

                let choice = plan_general_join_strategy(
                    left_plan.cost,
                    left_plan.rows,
                    left_ord.as_ref(),
                    &graph.relations[r].table_info,
                    right_scan,
                    &on_expr,
                    JoinType::Inner,
                    &left_accum_schema,
                    resource_sig,
                    row_goal,
                    required_ordering,
                );

                let mut cand_joins = left_plan.joins.clone();
                cand_joins.push(PhysicalJoin {
                    join_type: JoinType::Inner,
                    right: right_scan.clone(),
                    on: on_expr,
                    estimated_rows: join_rows,
                    cost: choice.cost,
                    strategy: choice.strategy,
                    index_name: choice.index_name,
                    runtime_filter: rf,
                    left_sort: choice.left_sort,
                    right_sort: choice.right_sort,
                    row_goal: choice.row_goal,
                    estimated_rows_examined: choice.estimated_rows_examined,
                    estimated_qualifying_rows: choice.estimated_qualifying_rows,
                    early_termination_possible: choice.early_termination_possible,
                    early_stop_phase: choice.early_stop_phase,
                });

                let mut cand_perm = left_plan.perm.clone();
                cand_perm.push(r);

                let cand_plan = DPPlan {
                    perm: cand_perm,
                    cost: choice.cost,
                    rows: join_rows,
                    base_scan: left_plan.base_scan.clone(),
                    joins: cand_joins,
                    used_predicates: cand_used_preds,
                    cartesian_joins: left_plan.cartesian_joins
                        + if has_direct_predicate { 0 } else { 1 },
                };

                best_for_mask = match best_for_mask {
                    None => Some(cand_plan),
                    Some(existing) => {
                        if is_better_plan(&cand_plan, &existing) {
                            Some(cand_plan)
                        } else {
                            Some(existing)
                        }
                    }
                };
            }

            dp[mask] = best_for_mask;
        }
    }

    dp[(1 << num_rels) - 1]
        .clone()
        .expect("DP plan must exist for full mask")
}

/// Optimizes an inner-join region using deterministic greedy search ($O(n^2)$ fallback for large graphs).
fn optimize_inner_join_region_greedy(
    graph: &JoinGraph,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
    row_goal: Option<&crate::query::physical_properties::RowGoal>,
    required_ordering: Option<&crate::query::physical_properties::RequiredOrdering>,
) -> DPPlan {
    let num_rels = graph.relations.len();
    assert!(num_rels > 0);

    // Step 1: Pick starting relation with minimum scan cardinality, then minimum scan cost, then lowest index
    let mut start_idx = 0;
    let mut min_rows = graph.relations[0].base_scan.estimated_rows;
    let mut min_cost = graph.relations[0].base_scan.cost;

    for i in 1..num_rels {
        let r_rows = graph.relations[i].base_scan.estimated_rows;
        let r_cost = graph.relations[i].base_scan.cost;
        if r_rows < min_rows - 1e-9
            || ((r_rows - min_rows).abs() <= 1e-9 && r_cost < min_cost - 1e-9)
        {
            start_idx = i;
            min_rows = r_rows;
            min_cost = r_cost;
        }
    }

    let mut visited = vec![false; num_rels];
    visited[start_idx] = true;
    let mut perm = vec![start_idx];
    let base_scan = graph.relations[start_idx].base_scan.clone();

    let mut used_predicates = vec![false; graph.predicates.len()];
    let mut joins = Vec::new();
    let mut cur_cost = base_scan.cost;
    let mut cur_rows = base_scan.estimated_rows;
    let mut cartesian_joins = 0;

    let mut available_rel_ids = HashSet::new();
    available_rel_ids.insert(start_idx);

    for _ in 1..num_rels {
        struct GreedyCandidate {
            cand: usize,
            join_cost: f64,
            join_rows: f64,
            has_direct_predicate: bool,
            step_conjuncts: Vec<Expr>,
            step_pred_indices: Vec<usize>,
        }

        let mut best_cand: Option<GreedyCandidate> = None;

        for (cand, &is_visited) in visited.iter().enumerate().take(num_rels) {
            if is_visited {
                continue;
            }

            let cand_scan = &graph.relations[cand].base_scan;
            let mut test_rel_ids = available_rel_ids.clone();
            test_rel_ids.insert(cand);

            let mut step_conjuncts = Vec::new();
            let mut step_pred_indices = Vec::new();
            let mut has_direct_predicate = false;

            for (p_idx, pred) in graph.predicates.iter().enumerate() {
                if !used_predicates[p_idx]
                    && pred.referenced_rel_ids.is_subset(&test_rel_ids)
                    && pred.referenced_rel_ids.contains(&cand)
                {
                    step_conjuncts.push(pred.expr.clone());
                    step_pred_indices.push(p_idx);
                    if pred
                        .referenced_rel_ids
                        .iter()
                        .any(|&id| id != cand && available_rel_ids.contains(&id))
                    {
                        has_direct_predicate = true;
                    }
                }
            }

            let on_expr = recombine_conjunction(step_conjuncts.clone())
                .unwrap_or(Expr::Literal(Value::Boolean(true)));

            let join_cost = CostModel::nested_loop_join_cost(
                cur_cost,
                cur_rows,
                cand_scan.cost,
                cand_scan.estimated_rows,
            );
            let mut join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
                cur_rows,
                graph.relations[start_idx].table_info.stats.as_ref(),
                cand_scan.estimated_rows,
                graph.relations[cand].table_info.stats.as_ref(),
                &on_expr,
                &graph.relations[cand].table_info.schema,
            );

            if let Some(fb_reg) = feedback {
                let key = FeedbackKey::join(
                    "Inner",
                    &graph.relations[start_idx].table_info.name,
                    &graph.relations[cand].table_info.name,
                    on_expr.to_string(),
                    param_sig.cloned(),
                    stats_gen,
                )
                .with_resource_signature(resource_sig.cloned());
                join_rows = fb_reg.get_adjusted_cardinality(&key, join_rows);
            }

            let is_better = match &best_cand {
                None => true,
                Some(best) => {
                    if has_direct_predicate != best.has_direct_predicate {
                        has_direct_predicate
                    } else if (join_cost - best.join_cost).abs() > 1e-9 {
                        join_cost < best.join_cost
                    } else if (join_rows - best.join_rows).abs() > 1e-9 {
                        join_rows < best.join_rows
                    } else {
                        cand < best.cand
                    }
                }
            };

            if is_better {
                best_cand = Some(GreedyCandidate {
                    cand,
                    join_cost,
                    join_rows,
                    has_direct_predicate,
                    step_conjuncts,
                    step_pred_indices,
                });
            }
        }

        let chosen = best_cand.unwrap();

        visited[chosen.cand] = true;
        perm.push(chosen.cand);
        available_rel_ids.insert(chosen.cand);

        for p_idx in chosen.step_pred_indices {
            used_predicates[p_idx] = true;
        }
        if !chosen.has_direct_predicate {
            cartesian_joins += 1;
        }

        let on_expr = recombine_conjunction(chosen.step_conjuncts)
            .unwrap_or(Expr::Literal(Value::Boolean(true)));

        let right_scan = graph.relations[chosen.cand].base_scan.clone();
        let mut left_cols = Vec::new();
        for &idx in &perm {
            left_cols.extend(graph.relations[idx].table_info.schema.columns.clone());
        }
        let left_accum_schema = Schema::new_projected(left_cols);

        let rf = plan_runtime_filter_for_join(
            JoinType::Inner,
            &on_expr,
            &left_accum_schema,
            &graph.relations[chosen.cand].table_info.schema,
            right_scan.estimated_rows,
            cur_rows,
            graph.relations[chosen.cand].table_info.stats.as_ref(),
            resource_sig,
        );

        let left_ord = if perm.len() == 1 {
            let first = perm[0];
            Some(crate::query::merge_join::ordering_property_for_table_scan(
                &graph.relations[first].table_info,
                &graph.relations[first].base_scan,
            ))
        } else {
            None
        };

        let choice = plan_general_join_strategy(
            cur_cost,
            cur_rows,
            left_ord.as_ref(),
            &graph.relations[chosen.cand].table_info,
            &right_scan,
            &on_expr,
            JoinType::Inner,
            &left_accum_schema,
            resource_sig,
            row_goal,
            required_ordering,
        );

        joins.push(PhysicalJoin {
            join_type: JoinType::Inner,
            right: right_scan,
            on: on_expr,
            estimated_rows: chosen.join_rows,
            cost: choice.cost,
            strategy: choice.strategy,
            index_name: choice.index_name,
            runtime_filter: rf,
            left_sort: choice.left_sort,
            right_sort: choice.right_sort,
            row_goal: choice.row_goal,
            estimated_rows_examined: choice.estimated_rows_examined,
            estimated_qualifying_rows: choice.estimated_qualifying_rows,
            early_termination_possible: choice.early_termination_possible,
            early_stop_phase: choice.early_stop_phase,
        });

        cur_cost = choice.cost;
        cur_rows = chosen.join_rows;
    }

    DPPlan {
        perm,
        cost: cur_cost,
        rows: cur_rows,
        base_scan,
        joins,
        used_predicates,
        cartesian_joins,
    }
}

/// Builds a `JoinGraph` for the given relations and candidate conjuncts.
#[allow(clippy::too_many_arguments)]
fn build_join_graph(
    region_rel_indices: &[usize],
    tables: &[TableOptInfo],
    candidate_conjuncts: &[Expr],
    bound: Option<&BoundSelect>,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
) -> (JoinGraph, Vec<Vec<Expr>>, Vec<Expr>) {
    let num_rels = region_rel_indices.len();
    let mut table_local_conjuncts: Vec<Vec<Expr>> = vec![Vec::new(); num_rels];
    let mut multi_conjuncts: Vec<Expr> = Vec::new();
    let mut unhandled_conjuncts: Vec<Expr> = Vec::new();

    let region_qualifiers: HashSet<String> = region_rel_indices
        .iter()
        .map(|&idx| tables[idx].qualifier().to_string())
        .collect();

    for conj in candidate_conjuncts {
        let refs = expr_referenced_qualifiers(conj);
        if refs.is_empty() {
            multi_conjuncts.push(conj.clone());
        } else if refs.len() == 1 {
            let target_qual = refs.iter().next().unwrap();
            if let Some(pos) = region_rel_indices
                .iter()
                .position(|&idx| tables[idx].qualifier() == target_qual)
            {
                table_local_conjuncts[pos].push(conj.clone());
            } else {
                unhandled_conjuncts.push(conj.clone());
            }
        } else if refs.is_subset(&region_qualifiers) {
            multi_conjuncts.push(conj.clone());
        } else {
            unhandled_conjuncts.push(conj.clone());
        }
    }

    let mut metrics = InferenceMetrics::default();
    let mut constraint_set =
        ConstraintSet::extract_from_conjuncts(candidate_conjuncts, &mut metrics);
    constraint_set.propagate_inner_joins(candidate_conjuncts, &mut metrics);

    let mut relations = Vec::with_capacity(num_rels);
    for (local_id, &global_idx) in region_rel_indices.iter().enumerate() {
        let t = &tables[global_idx];
        let qual = t.qualifier();
        let inferred_for_t = constraint_set.to_conjuncts(Some(qual));
        for inf_expr in inferred_for_t {
            if !table_local_conjuncts[local_id].contains(&inf_expr) {
                table_local_conjuncts[local_id].push(inf_expr);
            }
        }

        let ref_cols = bound.map(|b| collect_table_referenced_columns(t, b, candidate_conjuncts));
        let req_cols = bound.map(compute_query_required_columns);
        let base_scan = plan_table_scan(
            t,
            table_local_conjuncts[local_id].clone(),
            ref_cols.as_ref(),
            true,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
            req_cols.as_ref(),
        );
        relations.push(JoinGraphRelation {
            id: local_id,
            global_idx,
            qualifier: t.qualifier().to_string(),
            table_info: t.clone(),
            base_scan,
        });
    }

    let mut predicates = Vec::with_capacity(multi_conjuncts.len());
    for conj in multi_conjuncts {
        let refs = expr_referenced_qualifiers(&conj);
        let mut rel_ids = HashSet::new();
        for qual in &refs {
            if let Some(pos) = region_rel_indices
                .iter()
                .position(|&idx| tables[idx].qualifier() == qual)
            {
                rel_ids.insert(pos);
            }
        }

        let is_equality = match &conj {
            Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
            } => {
                let l_refs = expr_referenced_qualifiers(left);
                let r_refs = expr_referenced_qualifiers(right);
                l_refs.len() == 1 && r_refs.len() == 1 && l_refs != r_refs
            }
            _ => false,
        };

        predicates.push(JoinGraphPredicate {
            expr: conj,
            referenced_qualifiers: refs,
            referenced_rel_ids: rel_ids,
            is_equality,
        });
    }

    (
        JoinGraph {
            relations,
            predicates,
        },
        table_local_conjuncts,
        unhandled_conjuncts,
    )
}

/// Optimizes a bound query where all joins are INNER JOINs into an optimal reordered plan.
fn optimize_all_inner_joins(
    bound: &BoundSelect,
    tables: &[TableOptInfo],
    where_conjuncts: &[Expr],
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
) -> Result<PhysicalPlan> {
    let num_tables = tables.len();
    let mut all_conjuncts = where_conjuncts.to_vec();
    for j in &bound.joins {
        all_conjuncts.extend(split_conjunction(&j.on));
    }

    let mut metrics = InferenceMetrics::default();
    let mut constraint_set = ConstraintSet::extract_from_conjuncts(&all_conjuncts, &mut metrics);
    let join_on_clauses: Vec<Expr> = bound.joins.iter().map(|j| j.on.clone()).collect();
    constraint_set.propagate_inner_joins(&join_on_clauses, &mut metrics);

    if constraint_set.is_unsatisfiable() {
        let empty_scan =
            PhysicalTableScan::new_empty(tables[0].name.clone(), tables[0].alias.clone());
        return Ok(PhysicalPlan {
            ctes: bound.ctes.clone(),
            base_table: empty_scan,
            joins: Vec::new(),
            post_join_filter: None,
            group_by: bound.group_by.clone(),
            aggregates: bound.aggregates.clone(),
            aggregate_strategy: None,
            having: bound.having.clone(),
            window_exprs: bound.window_exprs.clone(),
            projections: bound.projections.clone(),
            order_by: bound.order_by.clone(),
            order_by_sort_required: false,
            order_by_top_n: false,
            provided_ordering: None,
            offset: bound.offset,
            limit: bound.limit,
            set_op: None,
            logical_rewrites: Vec::new(),
            inferred_predicates: constraint_set.inferences,
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            row_goal_early_termination: false,
            total_cost: 0.0,
            estimated_rows: 0.0,
        });
    }

    let region_indices: Vec<usize> = (0..num_tables).collect();
    let (graph, _table_locals, unhandled) = build_join_graph(
        &region_indices,
        tables,
        &all_conjuncts,
        Some(bound),
        feedback,
        param_sig,
        resource_sig,
        stats_gen,
    );

    let safe_row_goal =
        bound.group_by.is_empty() && bound.aggregates.is_empty() && bound.window_exprs.is_empty();
    let root_row_goal = if safe_row_goal {
        crate::query::physical_properties::RowGoal::from_limit_offset(bound.limit, bound.offset)
    } else {
        None
    };
    let required_ordering = if !bound.order_by.is_empty() {
        Some(
            crate::query::physical_properties::RequiredOrdering::from_sort_keys(
                &bound.order_by,
                None,
            ),
        )
    } else {
        None
    };

    let best_plan = if num_tables <= MAX_ENUMERATED_RELATIONS {
        optimize_inner_join_region_dp(
            &graph,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
            root_row_goal.as_ref(),
            required_ordering.as_ref(),
        )
    } else {
        optimize_inner_join_region_greedy(
            &graph,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
            root_row_goal.as_ref(),
            required_ordering.as_ref(),
        )
    };

    let mut remaining_post = unhandled;
    for (p_idx, pred) in graph.predicates.iter().enumerate() {
        if !best_plan.used_predicates[p_idx] {
            remaining_post.push(pred.expr.clone());
        }
    }

    let post_join_filter = recombine_conjunction(remaining_post);
    let mut cur_cost = best_plan.cost;
    let mut cur_rows = best_plan.rows;

    if let Some(pf) = &post_join_filter {
        cur_cost += CostModel::filter_cost(cur_rows);
        let sel = CardinalityEstimator::estimate_expr_selectivity(pf, None, &tables[0].schema);
        cur_rows = (cur_rows * sel).max(if cur_rows > 0.0 { 1.0 } else { 0.0 });
    }

    let mut current_ordering = crate::query::merge_join::ordering_property_for_table_scan(
        &tables[0],
        &best_plan.base_scan,
    );
    if !best_plan.joins.is_empty() {
        let last_join = best_plan.joins.last().unwrap();
        if last_join.strategy == JoinStrategy::Merge {
            if let Some(equi) = crate::query::external_hash_join::analyze_equi_join(
                &last_join.on,
                &tables[0].schema,
                tables
                    .get(1)
                    .map(|t| &t.schema)
                    .unwrap_or(&tables[0].schema),
            ) {
                let mut cols = Vec::new();
                for k in &equi.left_keys {
                    if let Expr::Column(col) = k {
                        cols.push(crate::query::merge_join::OrderingColumn {
                            column: col.clone(),
                            direction: crate::query::operator::SortDirection::Ascending,
                            nulls_first: true,
                        });
                    }
                }
                current_ordering = crate::query::merge_join::OrderingProperty::new(cols);
            } else {
                current_ordering = crate::query::merge_join::OrderingProperty::default();
            }
        } else {
            current_ordering = crate::query::merge_join::OrderingProperty::default();
        }
    }

    let (agg_strategy, post_agg_cost, post_agg_ordering) = plan_aggregation_strategy(
        cur_cost,
        cur_rows,
        Some(&current_ordering),
        &bound.group_by,
        &bound.aggregates,
        tables.first().map(|t| t.qualifier()),
        feedback,
        param_sig,
        resource_sig,
        stats_gen,
    );
    cur_cost = post_agg_cost;

    if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
        let dummy_schema = Schema::new_projected(vec![]);
        let schema_ref = tables.first().map(|t| &t.schema).unwrap_or(&dummy_schema);
        cur_rows = CardinalityEstimator::estimate_group_by_cardinality(
            cur_rows,
            &bound.group_by,
            tables.first().and_then(|t| t.stats.as_ref()),
            schema_ref,
        );
    }

    let mut current_ordering = post_agg_ordering;

    if let Some(having) = &bound.having {
        cur_cost += CostModel::filter_cost(cur_rows);
        let dummy_schema = Schema::new_projected(vec![]);
        let schema_ref = tables.first().map(|t| &t.schema).unwrap_or(&dummy_schema);
        let having_sel = CardinalityEstimator::estimate_expr_selectivity(
            having,
            tables.first().and_then(|t| t.stats.as_ref()),
            schema_ref,
        );
        cur_rows = (cur_rows * having_sel).clamp(if cur_rows > 0.0 { 1.0 } else { 0.0 }, cur_rows);
    }

    if !bound.window_exprs.is_empty() {
        cur_cost += CostModel::window_cost(cur_rows);
        if let Some(w) = bound.window_exprs.first() {
            let mut win_cols = Vec::new();
            for p in &w.window_spec.partition_by {
                if let Expr::Column(col) = p {
                    win_cols.push(crate::query::physical_properties::OrderingColumn {
                        column: col.clone(),
                        direction: crate::query::operator::SortDirection::Ascending,
                        nulls_first: true,
                    });
                }
            }
            for o in &w.window_spec.order_by {
                if let Expr::Column(col) = &o.expression {
                    win_cols.push(crate::query::physical_properties::OrderingColumn {
                        column: col.clone(),
                        direction: o.direction,
                        nulls_first: o.nulls_first.unwrap_or(
                            o.direction == crate::query::operator::SortDirection::Ascending,
                        ),
                    });
                }
            }
            if !win_cols.is_empty() {
                current_ordering =
                    crate::query::physical_properties::OrderingProperty::new(win_cols);
            }
        }
    }

    let post_proj_ordering = current_ordering.project(&bound.projections);
    current_ordering = post_proj_ordering;

    let mut sort_required = false;
    let mut top_n = false;
    if !bound.order_by.is_empty() {
        let sort_budget = resource_sig
            .map(|r| r.sort_budget)
            .unwrap_or(DEFAULT_SORT_MEMORY_BUDGET_BYTES);
        let key = FeedbackKey::new(
            FeedbackContext::Sort {
                order_by: bound
                    .order_by
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            param_sig.cloned(),
            stats_gen,
        )
        .with_resource_signature(resource_sig.cloned());

        let is_already_sorted =
            current_ordering.satisfies_order_by(&bound.order_by, None, Some(&bound.projections));
        if is_already_sorted {
            sort_required = false;
            top_n = false;
        } else {
            sort_required = true;
            let sort_cost = if let Some(fb_reg) = feedback {
                let spill_status = fb_reg.get_spill_status(&key);
                let adj_rows = fb_reg.get_adjusted_cardinality(&key, cur_rows);
                CostModel::sort_cost_with_budget(adj_rows, 64.0, sort_budget, spill_status)
            } else {
                CostModel::sort_cost_with_budget(cur_rows, 64.0, sort_budget, SpillStatus::Unknown)
            };
            if let Some(limit_val) = bound.limit {
                let k = bound.offset.unwrap_or(0) + limit_val;
                let top_n_cost = CostModel::top_n_cost(cur_rows, k, 64.0);
                if top_n_cost < sort_cost {
                    top_n = true;
                    cur_cost += top_n_cost;
                } else {
                    top_n = false;
                    cur_cost += sort_cost;
                }
            } else {
                top_n = false;
                cur_cost += sort_cost;
            }
            current_ordering = crate::query::physical_properties::OrderingProperty::from_sort_keys(
                &bound.order_by,
            );
        }
    }

    if let Some(limit_val) = bound.limit {
        let off = bound.offset.unwrap_or(0) as f64;
        cur_rows = ((cur_rows - off).max(0.0)).min(limit_val as f64);
    }

    Ok(PhysicalPlan {
        ctes: bound.ctes.clone(),
        base_table: best_plan.base_scan,
        joins: best_plan.joins,
        post_join_filter,
        group_by: bound.group_by.clone(),
        aggregates: bound.aggregates.clone(),
        aggregate_strategy: agg_strategy,
        having: bound.having.clone(),
        window_exprs: bound.window_exprs.clone(),
        projections: bound.projections.clone(),
        order_by: bound.order_by.clone(),
        order_by_sort_required: sort_required,
        order_by_top_n: top_n,
        provided_ordering: Some(current_ordering),
        offset: bound.offset,
        limit: bound.limit,
        set_op: None,
        logical_rewrites: Vec::new(),
        inferred_predicates: constraint_set.inferences,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        row_goal_early_termination: false,
        total_cost: cur_cost,
        estimated_rows: cur_rows,
    })
}

/// Optimizes a bound query with OUTER JOIN barriers, preserving outer join boundaries.
fn optimize_with_outer_barriers(
    bound: &BoundSelect,
    tables: &[TableOptInfo],
    where_conjuncts: &[Expr],
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
) -> Result<PhysicalPlan> {
    let is_multi_table = !bound.joins.is_empty();
    let mut table_local_conjuncts: Vec<Vec<Expr>> = vec![Vec::new(); tables.len()];
    let mut post_join_conjuncts: Vec<Expr> = Vec::new();

    let mut metrics = InferenceMetrics::default();
    let mut constraint_set = ConstraintSet::extract_from_conjuncts(where_conjuncts, &mut metrics);
    let inner_on_clauses: Vec<Expr> = bound
        .joins
        .iter()
        .filter(|j| j.join_type == JoinType::Inner)
        .map(|j| j.on.clone())
        .collect();
    constraint_set.propagate_inner_joins(&inner_on_clauses, &mut metrics);

    if constraint_set.is_unsatisfiable() {
        let empty_scan =
            PhysicalTableScan::new_empty(tables[0].name.clone(), tables[0].alias.clone());
        return Ok(PhysicalPlan {
            ctes: bound.ctes.clone(),
            base_table: empty_scan,
            joins: Vec::new(),
            post_join_filter: None,
            group_by: bound.group_by.clone(),
            aggregates: bound.aggregates.clone(),
            aggregate_strategy: None,
            having: bound.having.clone(),
            window_exprs: bound.window_exprs.clone(),
            projections: bound.projections.clone(),
            order_by: bound.order_by.clone(),
            order_by_sort_required: false,
            order_by_top_n: false,
            provided_ordering: None,
            offset: bound.offset,
            limit: bound.limit,
            set_op: None,
            logical_rewrites: Vec::new(),
            inferred_predicates: constraint_set.inferences,
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            row_goal_early_termination: false,
            total_cost: 0.0,
            estimated_rows: 0.0,
        });
    }

    // 1. Classify WHERE conjuncts per table with outer join pushdown safety
    for conjunct in where_conjuncts {
        let refs = expr_referenced_qualifiers(conjunct);
        if refs.len() == 1 {
            let target_qualifier = refs.iter().next().unwrap();
            if let Some(tbl_idx) = tables
                .iter()
                .position(|t| t.qualifier() == target_qualifier)
            {
                if tbl_idx == 0 {
                    let has_right_or_full = bound
                        .joins
                        .iter()
                        .any(|j| j.join_type == JoinType::Right || j.join_type == JoinType::Full);
                    if has_right_or_full {
                        post_join_conjuncts.push(conjunct.clone());
                    } else {
                        table_local_conjuncts[0].push(conjunct.clone());
                    }
                } else {
                    let this_join_type = bound.joins[tbl_idx - 1].join_type;
                    let is_outer_right =
                        this_join_type == JoinType::Left || this_join_type == JoinType::Full;
                    let subsequent_outer = bound.joins[tbl_idx..]
                        .iter()
                        .any(|j| j.join_type == JoinType::Right || j.join_type == JoinType::Full);
                    if is_outer_right || subsequent_outer {
                        post_join_conjuncts.push(conjunct.clone());
                    } else {
                        table_local_conjuncts[tbl_idx].push(conjunct.clone());
                    }
                }
            } else {
                post_join_conjuncts.push(conjunct.clone());
            }
        } else {
            post_join_conjuncts.push(conjunct.clone());
        }
    }

    // Add inferred constraints for table 0 if any
    let inferred_for_base = constraint_set.to_conjuncts(Some(tables[0].qualifier()));
    for inf_expr in inferred_for_base {
        if !table_local_conjuncts[0].contains(&inf_expr) {
            table_local_conjuncts[0].push(inf_expr);
        }
    }

    // 2. Process JOIN ON clauses
    let mut optimized_joins = Vec::new();
    for (join_idx, bound_join) in bound.joins.iter().enumerate() {
        let right_table_idx = join_idx + 1;
        let right_qualifier = tables[right_table_idx].qualifier();
        let on_conjuncts = split_conjunction(&bound_join.on);
        let mut remaining_on = Vec::new();

        for on_conj in on_conjuncts {
            let refs = expr_referenced_qualifiers(&on_conj);
            if bound_join.join_type == JoinType::Inner {
                if refs.len() == 1 && refs.contains(right_qualifier) {
                    table_local_conjuncts[right_table_idx].push(on_conj);
                } else if refs.len() == 1 && refs.contains(tables[0].qualifier()) {
                    table_local_conjuncts[0].push(on_conj);
                } else {
                    remaining_on.push(on_conj);
                }
            } else if bound_join.join_type == JoinType::Left {
                if refs.len() == 1 && refs.contains(right_qualifier) {
                    table_local_conjuncts[right_table_idx].push(on_conj);
                } else {
                    remaining_on.push(on_conj);
                }
            } else if bound_join.join_type == JoinType::Right {
                if refs.len() == 1 && refs.contains(tables[0].qualifier()) {
                    table_local_conjuncts[0].push(on_conj);
                } else {
                    remaining_on.push(on_conj);
                }
            } else {
                // Full join: cannot push down on predicates to either side
                remaining_on.push(on_conj);
            }
        }

        let on_expr =
            recombine_conjunction(remaining_on).unwrap_or(Expr::Literal(Value::Boolean(true)));
        optimized_joins.push((bound_join.join_type, on_expr));
    }

    // 3. Plan Base Table Scan
    let req_cols_all = compute_query_required_columns(bound);
    let ref_cols_base = collect_table_referenced_columns(&tables[0], bound, where_conjuncts);
    let base_table_scan = plan_table_scan(
        &tables[0],
        table_local_conjuncts[0].clone(),
        Some(&ref_cols_base),
        is_multi_table,
        feedback,
        param_sig,
        resource_sig,
        stats_gen,
        Some(&req_cols_all),
    );

    let mut physical_joins = Vec::new();
    let mut cur_cost = base_table_scan.cost;
    let mut cur_rows = base_table_scan.estimated_rows;
    let mut accum_schema = tables[0].schema.clone();

    for (join_idx, (join_type, on_expr)) in optimized_joins.into_iter().enumerate() {
        let right_table_idx = join_idx + 1;
        let right_qual = tables[right_table_idx].qualifier();
        if join_type == JoinType::Inner {
            let inferred_for_right = constraint_set.to_conjuncts(Some(right_qual));
            for inf_expr in inferred_for_right {
                if !table_local_conjuncts[right_table_idx].contains(&inf_expr) {
                    table_local_conjuncts[right_table_idx].push(inf_expr);
                }
            }
        }
        let ref_cols_right =
            collect_table_referenced_columns(&tables[right_table_idx], bound, where_conjuncts);
        let right_table_scan = plan_table_scan(
            &tables[right_table_idx],
            table_local_conjuncts[right_table_idx].clone(),
            Some(&ref_cols_right),
            is_multi_table,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
            Some(&req_cols_all),
        );

        let mut join_rows = CardinalityEstimator::estimate_join_cardinality(
            join_type,
            cur_rows,
            tables[0].stats.as_ref(),
            right_table_scan.estimated_rows,
            tables[right_table_idx].stats.as_ref(),
            &on_expr,
            &tables[right_table_idx].schema,
        );

        if let Some(fb_reg) = feedback {
            let left_name = &tables[0].name;
            let right_name = &tables[right_table_idx].name;
            let key = if join_type == JoinType::Semi || join_type == JoinType::Anti {
                FeedbackKey::new(
                    FeedbackContext::SemiAnti {
                        strategy: "Default".to_string(),
                        left_table: left_name.clone(),
                        right_table: right_name.clone(),
                        on_clause: on_expr.to_string(),
                    },
                    param_sig.cloned(),
                    stats_gen,
                )
                .with_resource_signature(resource_sig.cloned())
            } else {
                FeedbackKey::join(
                    format!("{join_type:?}"),
                    left_name,
                    right_name,
                    on_expr.to_string(),
                    param_sig.cloned(),
                    stats_gen,
                )
                .with_resource_signature(resource_sig.cloned())
            };
            join_rows = fb_reg.get_adjusted_cardinality(&key, join_rows);
        }

        let safe_row_goal = bound.group_by.is_empty()
            && bound.aggregates.is_empty()
            && bound.window_exprs.is_empty();
        let root_row_goal = if safe_row_goal {
            crate::query::physical_properties::RowGoal::from_limit_offset(bound.limit, bound.offset)
        } else {
            None
        };
        let required_ordering = if !bound.order_by.is_empty() {
            Some(
                crate::query::physical_properties::RequiredOrdering::from_sort_keys(
                    &bound.order_by,
                    None,
                ),
            )
        } else {
            None
        };

        let (
            chosen_strategy,
            chosen_index,
            chosen_cost,
            chosen_left_sort,
            chosen_right_sort,
            chosen_row_goal,
            chosen_examined,
            chosen_qualifying,
            chosen_early,
            chosen_phase,
        ) = if join_type == JoinType::Semi || join_type == JoinType::Anti {
            let (strat, idx, c, examined, qualifying, early, phase) = plan_semi_anti_strategy(
                cur_cost,
                cur_rows,
                &tables[0],
                &tables[right_table_idx],
                &right_table_scan,
                &on_expr,
                join_type,
                feedback,
                param_sig,
                resource_sig,
                stats_gen,
                root_row_goal.as_ref(),
            );
            (
                strat,
                idx,
                c,
                false,
                false,
                root_row_goal,
                examined,
                qualifying,
                early,
                phase,
            )
        } else {
            let left_ord = if physical_joins.is_empty() {
                Some(crate::query::merge_join::ordering_property_for_table_scan(
                    &tables[0],
                    &base_table_scan,
                ))
            } else {
                None
            };
            let choice = plan_general_join_strategy(
                cur_cost,
                cur_rows,
                left_ord.as_ref(),
                &tables[right_table_idx],
                &right_table_scan,
                &on_expr,
                join_type,
                &accum_schema,
                resource_sig,
                root_row_goal.as_ref(),
                required_ordering.as_ref(),
            );
            (
                choice.strategy,
                choice.index_name,
                choice.cost,
                choice.left_sort,
                choice.right_sort,
                choice.row_goal,
                choice.estimated_rows_examined,
                choice.estimated_qualifying_rows,
                choice.early_termination_possible,
                choice.early_stop_phase,
            )
        };

        let rf = plan_runtime_filter_for_join(
            join_type,
            &on_expr,
            &accum_schema,
            &tables[right_table_idx].schema,
            right_table_scan.estimated_rows,
            cur_rows,
            tables[right_table_idx].stats.as_ref(),
            resource_sig,
        );

        let mut next_cols = accum_schema.columns.clone();
        next_cols.extend(tables[right_table_idx].schema.columns.clone());
        accum_schema = Schema::new_projected(next_cols);

        cur_cost = chosen_cost;
        cur_rows = join_rows;

        physical_joins.push(PhysicalJoin {
            join_type,
            right: right_table_scan,
            on: on_expr,
            estimated_rows: join_rows,
            cost: chosen_cost,
            strategy: chosen_strategy,
            index_name: chosen_index,
            runtime_filter: rf,
            left_sort: chosen_left_sort,
            right_sort: chosen_right_sort,
            row_goal: chosen_row_goal,
            estimated_rows_examined: chosen_examined,
            estimated_qualifying_rows: chosen_qualifying,
            early_termination_possible: chosen_early,
            early_stop_phase: chosen_phase,
        });
    }

    let post_join_filter = recombine_conjunction(post_join_conjuncts);
    if let Some(post_f) = &post_join_filter {
        cur_cost += CostModel::filter_cost(cur_rows);
        let sel = CardinalityEstimator::estimate_expr_selectivity(post_f, None, &tables[0].schema);
        cur_rows = (cur_rows * sel).max(if cur_rows > 0.0 { 1.0 } else { 0.0 });
    }

    let mut current_ordering =
        crate::query::merge_join::ordering_property_for_table_scan(&tables[0], &base_table_scan);
    if !physical_joins.is_empty() {
        let last_join = physical_joins.last().unwrap();
        if last_join.strategy == JoinStrategy::Merge {
            if let Some(equi) = crate::query::external_hash_join::analyze_equi_join(
                &last_join.on,
                &tables[0].schema,
                tables
                    .get(1)
                    .map(|t| &t.schema)
                    .unwrap_or(&tables[0].schema),
            ) {
                let mut cols = Vec::new();
                for k in &equi.left_keys {
                    if let Expr::Column(col) = k {
                        cols.push(crate::query::merge_join::OrderingColumn {
                            column: col.clone(),
                            direction: crate::query::operator::SortDirection::Ascending,
                            nulls_first: true,
                        });
                    }
                }
                current_ordering = crate::query::merge_join::OrderingProperty::new(cols);
            } else {
                current_ordering = crate::query::merge_join::OrderingProperty::default();
            }
        } else {
            current_ordering = crate::query::merge_join::OrderingProperty::default();
        }
    }

    let (agg_strategy, post_agg_cost, post_agg_ordering) = plan_aggregation_strategy(
        cur_cost,
        cur_rows,
        Some(&current_ordering),
        &bound.group_by,
        &bound.aggregates,
        tables.first().map(|t| t.qualifier()),
        feedback,
        param_sig,
        resource_sig,
        stats_gen,
    );
    cur_cost = post_agg_cost;

    if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
        let dummy_schema = Schema::new_projected(vec![]);
        let schema_ref = tables.first().map(|t| &t.schema).unwrap_or(&dummy_schema);
        cur_rows = CardinalityEstimator::estimate_group_by_cardinality(
            cur_rows,
            &bound.group_by,
            tables.first().and_then(|t| t.stats.as_ref()),
            schema_ref,
        );
    }

    let mut current_ordering = post_agg_ordering;

    if let Some(having) = &bound.having {
        cur_cost += CostModel::filter_cost(cur_rows);
        let dummy_schema = Schema::new_projected(vec![]);
        let schema_ref = tables.first().map(|t| &t.schema).unwrap_or(&dummy_schema);
        let having_sel = CardinalityEstimator::estimate_expr_selectivity(
            having,
            tables.first().and_then(|t| t.stats.as_ref()),
            schema_ref,
        );
        cur_rows = (cur_rows * having_sel).clamp(if cur_rows > 0.0 { 1.0 } else { 0.0 }, cur_rows);
    }

    if !bound.window_exprs.is_empty() {
        cur_cost += CostModel::window_cost(cur_rows);
        if let Some(w) = bound.window_exprs.first() {
            let mut win_cols = Vec::new();
            for p in &w.window_spec.partition_by {
                if let Expr::Column(col) = p {
                    win_cols.push(crate::query::physical_properties::OrderingColumn {
                        column: col.clone(),
                        direction: crate::query::operator::SortDirection::Ascending,
                        nulls_first: true,
                    });
                }
            }
            for o in &w.window_spec.order_by {
                if let Expr::Column(col) = &o.expression {
                    win_cols.push(crate::query::physical_properties::OrderingColumn {
                        column: col.clone(),
                        direction: o.direction,
                        nulls_first: o.nulls_first.unwrap_or(
                            o.direction == crate::query::operator::SortDirection::Ascending,
                        ),
                    });
                }
            }
            if !win_cols.is_empty() {
                current_ordering =
                    crate::query::physical_properties::OrderingProperty::new(win_cols);
            }
        }
    }

    let post_proj_ordering = current_ordering.project(&bound.projections);
    current_ordering = post_proj_ordering;

    let mut sort_required = false;
    let mut top_n = false;
    if !bound.order_by.is_empty() {
        let sort_budget = resource_sig
            .map(|r| r.sort_budget)
            .unwrap_or(DEFAULT_SORT_MEMORY_BUDGET_BYTES);
        let key = FeedbackKey::new(
            FeedbackContext::Sort {
                order_by: bound
                    .order_by
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            param_sig.cloned(),
            stats_gen,
        )
        .with_resource_signature(resource_sig.cloned());

        let is_already_sorted =
            current_ordering.satisfies_order_by(&bound.order_by, None, Some(&bound.projections));
        if is_already_sorted {
            sort_required = false;
            top_n = false;
        } else {
            sort_required = true;
            let sort_cost = if let Some(fb_reg) = feedback {
                let spill_status = fb_reg.get_spill_status(&key);
                let adj_rows = fb_reg.get_adjusted_cardinality(&key, cur_rows);
                CostModel::sort_cost_with_budget(adj_rows, 64.0, sort_budget, spill_status)
            } else {
                CostModel::sort_cost_with_budget(cur_rows, 64.0, sort_budget, SpillStatus::Unknown)
            };
            if let Some(limit_val) = bound.limit {
                let k = bound.offset.unwrap_or(0) + limit_val;
                let top_n_cost = CostModel::top_n_cost(cur_rows, k, 64.0);
                if top_n_cost < sort_cost {
                    top_n = true;
                    cur_cost += top_n_cost;
                } else {
                    top_n = false;
                    cur_cost += sort_cost;
                }
            } else {
                top_n = false;
                cur_cost += sort_cost;
            }
            current_ordering = crate::query::physical_properties::OrderingProperty::from_sort_keys(
                &bound.order_by,
            );
        }
    }

    if let Some(limit_val) = bound.limit {
        let off = bound.offset.unwrap_or(0) as f64;
        cur_rows = ((cur_rows - off).max(0.0)).min(limit_val as f64);
    }

    Ok(PhysicalPlan {
        ctes: bound.ctes.clone(),
        base_table: base_table_scan,
        joins: physical_joins,
        post_join_filter,
        group_by: bound.group_by.clone(),
        aggregates: bound.aggregates.clone(),
        aggregate_strategy: agg_strategy,
        having: bound.having.clone(),
        window_exprs: bound.window_exprs.clone(),
        projections: bound.projections.clone(),
        order_by: bound.order_by.clone(),
        order_by_sort_required: sort_required,
        order_by_top_n: top_n,
        provided_ordering: Some(current_ordering),
        offset: bound.offset,
        limit: bound.limit,
        set_op: None,
        logical_rewrites: Vec::new(),
        inferred_predicates: constraint_set.inferences,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        row_goal_early_termination: false,
        total_cost: cur_cost,
        estimated_rows: cur_rows,
    })
}

/// Plans an optimal runtime join filter for eligible inner/semi joins.
#[allow(clippy::too_many_arguments)]
fn plan_runtime_filter_for_join(
    join_type: JoinType,
    on_expr: &Expr,
    left_schema: &Schema,
    right_schema: &Schema,
    build_rows: f64,
    probe_rows: f64,
    build_stats: Option<&TableStatistics>,
    resource_sig: Option<&ResourcePlanningSignature>,
) -> Option<crate::query::runtime_filter::RuntimeFilterPlan> {
    if join_type != JoinType::Inner && join_type != JoinType::Semi {
        return None;
    }
    let join_budget = resource_sig
        .map(|r| r.join_budget)
        .unwrap_or(crate::query::external_hash_join::DEFAULT_JOIN_MEMORY_BUDGET_BYTES);

    let equi_plan =
        crate::query::external_hash_join::analyze_equi_join(on_expr, left_schema, right_schema)?;

    let mut build_ndv = None;
    if let Some(stats) = build_stats {
        if equi_plan.right_keys.len() == 1 {
            if let Expr::Column(col_name) = &equi_plan.right_keys[0] {
                let bare_col = col_name.split('.').next_back().unwrap_or(col_name);
                if let Some(col_stats) = stats.column(bare_col) {
                    build_ndv = Some(col_stats.distinct_count.max(1) as f64);
                }
            }
        }
    }

    let config =
        crate::query::external_hash_join::JoinConfig::new().with_memory_budget_bytes(join_budget);

    crate::query::runtime_filter::should_build_runtime_filter(
        join_type,
        &equi_plan.right_keys,
        build_rows,
        probe_rows,
        build_ndv,
        join_budget,
        &config,
    )
}

/// Selected physical join algorithm and computed cost.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedJoinChoice {
    pub strategy: JoinStrategy,
    pub index_name: Option<String>,
    pub cost: f64,
    pub left_sort: bool,
    pub right_sort: bool,
    pub row_goal: Option<crate::query::physical_properties::RowGoal>,
    pub estimated_rows_examined: Option<f64>,
    pub estimated_qualifying_rows: Option<f64>,
    pub early_termination_possible: bool,
    pub early_stop_phase: Option<String>,
}

/// Evaluates candidate physical join strategies (NestedLoop, HashJoin, MergeJoin)
/// and selects the cheapest valid strategy based on the cost model.
#[allow(clippy::too_many_arguments)]
pub fn plan_general_join_strategy(
    left_cost: f64,
    left_rows: f64,
    left_ordering: Option<&crate::query::merge_join::OrderingProperty>,
    right_table: &TableOptInfo,
    right_scan: &PhysicalTableScan,
    on: &Expr,
    join_type: JoinType,
    left_schema: &Schema,
    resource_sig: Option<&ResourcePlanningSignature>,
    row_goal: Option<&crate::query::physical_properties::RowGoal>,
    required_ordering: Option<&crate::query::physical_properties::RequiredOrdering>,
) -> PlannedJoinChoice {
    let join_budget = resource_sig
        .map(|r| r.join_budget)
        .unwrap_or(crate::query::external_hash_join::DEFAULT_JOIN_MEMORY_BUDGET_BYTES);
    let sort_budget = resource_sig
        .map(|r| r.sort_budget)
        .unwrap_or(crate::query::external_sort::DEFAULT_SORT_MEMORY_BUDGET_BYTES);

    let est_join_rows = CardinalityEstimator::estimate_join_cardinality(
        join_type,
        left_rows,
        None,
        right_scan.estimated_rows,
        right_table.stats.as_ref(),
        on,
        &right_table.schema,
    );

    let is_inner = join_type == JoinType::Inner;

    // 1. NestedLoop Strategy
    let nl_ordering_compatible = match required_ordering {
        Some(req) if !req.is_empty() => left_ordering
            .map(|ord| ord.satisfies_required_ordering(req))
            .unwrap_or(false),
        _ => true,
    };
    let nl_can_early_stop = is_inner && nl_ordering_compatible;

    let (cost_nl, nl_examined, nl_early, nl_phase) = match (row_goal, nl_can_early_stop) {
        (Some(rg), true) => {
            let k = rg.k() as f64;
            let probes =
                CostModel::expected_outer_probes_for_join_row_goal(k, est_join_rows, left_rows);
            let cost = CostModel::nested_loop_join_row_goal_cost(
                left_cost,
                probes,
                right_scan.cost,
                right_scan.estimated_rows,
                k,
            );
            (cost, Some(probes), true, Some("possible".to_string()))
        }
        _ => {
            let cost = CostModel::nested_loop_join_cost(
                left_cost,
                left_rows,
                right_scan.cost,
                right_scan.estimated_rows,
            );
            (cost, None, false, None)
        }
    };

    let mut best_strategy = JoinStrategy::NestedLoop;
    let mut best_cost = cost_nl;
    let mut best_left_sort = false;
    let mut best_right_sort = false;
    let mut best_examined = nl_examined;
    let mut best_early = nl_early;
    let mut best_phase = nl_phase;

    // 2. If equi-join conjuncts exist, evaluate Hash and Merge
    if let Some(equi_plan) =
        crate::query::external_hash_join::analyze_equi_join(on, left_schema, &right_table.schema)
    {
        // Hash Join candidate
        let hash_ordering_compatible = !matches!(required_ordering, Some(req) if !req.is_empty());
        let hash_can_early_stop = is_inner && hash_ordering_compatible;

        let is_external = right_scan.estimated_rows * 64.0 > join_budget as f64;
        let (cost_hash, hash_examined, hash_early, hash_phase) =
            match (row_goal, hash_can_early_stop) {
                (Some(rg), true) => {
                    let k = rg.k() as f64;
                    let probes = CostModel::expected_outer_probes_for_join_row_goal(
                        k,
                        est_join_rows,
                        left_rows,
                    );
                    let cost = if is_external {
                        CostModel::external_hash_join_row_goal_cost(
                            left_cost,
                            probes,
                            right_scan.cost,
                            right_scan.estimated_rows,
                            join_budget,
                            k,
                        )
                    } else {
                        CostModel::hash_join_row_goal_cost(
                            left_cost,
                            probes,
                            right_scan.cost,
                            right_scan.estimated_rows,
                            join_budget,
                            k,
                        )
                    };
                    (
                        cost,
                        Some(right_scan.estimated_rows + probes),
                        true,
                        Some("probe phase".to_string()),
                    )
                }
                _ => {
                    let cost = CostModel::hash_join_cost(
                        left_cost,
                        left_rows,
                        right_scan.cost,
                        right_scan.estimated_rows,
                        join_budget,
                    );
                    (cost, None, false, None)
                }
            };

        if cost_hash < best_cost {
            best_strategy = JoinStrategy::Hash;
            best_cost = cost_hash;
            best_left_sort = false;
            best_right_sort = false;
            best_examined = hash_examined;
            best_early = hash_early;
            best_phase = hash_phase;
        }

        // Merge Join candidate
        let right_ord =
            crate::query::merge_join::ordering_property_for_table_scan(right_table, right_scan);
        let right_has_order =
            right_ord.satisfies_join_keys(&equi_plan.right_keys, Some(right_scan.qualifier()));
        let right_needs_sort = !right_has_order;

        let left_has_order = left_ordering
            .map(|ord| ord.satisfies_join_keys(&equi_plan.left_keys, None))
            .unwrap_or(false);
        let left_needs_sort = !left_has_order;

        // Check whether merge join ordering satisfies required ordering
        let merge_ordering_compatible = match required_ordering {
            Some(req) if !req.is_empty() => {
                let join_output_order = crate::query::physical_properties::OrderingProperty::new(
                    equi_plan
                        .left_keys
                        .iter()
                        .filter_map(|k| {
                            if let Expr::Column(c) = k {
                                Some(crate::query::physical_properties::OrderingColumn {
                                    column: c.clone(),
                                    direction: crate::query::operator::SortDirection::Ascending,
                                    nulls_first: true,
                                })
                            } else {
                                None
                            }
                        })
                        .collect(),
                );
                join_output_order.satisfies_required_ordering(req)
            }
            _ => true,
        };

        let can_early_stop_merge = is_inner && merge_ordering_compatible;

        let (cost_merge, merge_examined, merge_early, merge_phase) =
            match (row_goal, can_early_stop_merge) {
                (Some(rg), true) => {
                    let k = rg.k() as f64;
                    let probes = CostModel::expected_outer_probes_for_join_row_goal(
                        k,
                        est_join_rows,
                        left_rows,
                    );
                    let merge_examined = (probes
                        + (probes / left_rows.max(1.0)) * right_scan.estimated_rows)
                        .min(left_rows + right_scan.estimated_rows);
                    let cost = CostModel::merge_join_row_goal_cost(
                        left_cost,
                        left_rows,
                        left_needs_sort,
                        right_scan.cost,
                        right_scan.estimated_rows,
                        right_needs_sort,
                        equi_plan.residual_predicate.is_some(),
                        sort_budget,
                        merge_examined,
                        k,
                    );
                    (
                        cost,
                        Some(merge_examined),
                        true,
                        Some("possible".to_string()),
                    )
                }
                _ => {
                    let cost = CostModel::merge_join_cost(
                        left_cost,
                        left_rows,
                        left_needs_sort,
                        right_scan.cost,
                        right_scan.estimated_rows,
                        right_needs_sort,
                        equi_plan.residual_predicate.is_some(),
                        sort_budget,
                    );
                    (cost, None, false, None)
                }
            };

        if cost_merge < best_cost {
            best_strategy = JoinStrategy::Merge;
            best_cost = cost_merge;
            best_left_sort = left_needs_sort;
            best_right_sort = right_needs_sort;
            best_examined = merge_examined;
            best_early = merge_early;
            best_phase = merge_phase;
        }
    }

    PlannedJoinChoice {
        strategy: best_strategy,
        index_name: None,
        cost: best_cost,
        left_sort: best_left_sort,
        right_sort: best_right_sort,
        row_goal: if best_early { row_goal.cloned() } else { None },
        estimated_rows_examined: best_examined,
        estimated_qualifying_rows: if best_early {
            row_goal.map(|rg| (rg.k() as f64).min(est_join_rows))
        } else {
            None
        },
        early_termination_possible: best_early,
        early_stop_phase: best_phase,
    }
}

/// Evaluates costs of applicable physical strategies for SEMI/ANTI joins and selects the cheapest.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn plan_semi_anti_strategy(
    left_cost: f64,
    left_rows: f64,
    left_table: &TableOptInfo,
    right_table: &TableOptInfo,
    right_scan: &PhysicalTableScan,
    on_expr: &Expr,
    join_type: JoinType,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
    row_goal: Option<&crate::query::physical_properties::RowGoal>,
) -> (
    JoinStrategy,
    Option<String>,
    f64,
    Option<f64>,
    Option<f64>,
    bool,
    Option<String>,
) {
    let mut match_prob = CardinalityEstimator::estimate_semi_match_probability(
        left_rows,
        left_table.stats.as_ref(),
        right_scan.estimated_rows,
        right_table.stats.as_ref(),
        on_expr,
        &right_table.schema,
    );

    if let Some(fb_reg) = feedback {
        let key = FeedbackKey::new(
            FeedbackContext::SemiAnti {
                strategy: format!("{join_type:?}"),
                left_table: left_table.name.clone(),
                right_table: right_table.name.clone(),
                on_clause: on_expr.to_string(),
            },
            param_sig.cloned(),
            stats_gen,
        )
        .with_resource_signature(resource_sig.cloned());
        if let Some(fb) = fb_reg.get_feedback(&key) {
            if left_rows > 0.0 {
                match_prob = (fb.effective_cardinality / left_rows).clamp(0.0, 1.0);
            }
        }
    }

    let expected_probes = if let Some(rg) = row_goal {
        let k = rg.k() as f64;
        if join_type == JoinType::Semi {
            (k / match_prob.max(0.001)).min(left_rows)
        } else {
            (k / (1.0 - match_prob).max(0.001)).min(left_rows)
        }
    } else {
        left_rows
    };

    // 1. NestedLoop Strategy
    let nl_cost = if let Some(rg) = row_goal {
        CostModel::nested_loop_join_row_goal_cost(
            left_cost,
            expected_probes,
            right_scan.cost,
            right_scan.estimated_rows,
            rg.k() as f64,
        )
    } else if join_type == JoinType::Semi {
        CostModel::nested_loop_semi_cost(
            left_cost,
            left_rows,
            right_scan.cost,
            right_scan.estimated_rows,
            match_prob,
        )
    } else {
        CostModel::nested_loop_anti_cost(
            left_cost,
            left_rows,
            right_scan.cost,
            right_scan.estimated_rows,
            match_prob,
        )
    };

    let mut best_strategy = JoinStrategy::NestedLoop;
    let mut best_index = None;
    let mut best_cost = nl_cost;

    // Check if on_expr contains an equi-join condition
    let on_conjuncts = split_conjunction(on_expr);
    let right_qualifier = right_table.qualifier();
    let mut equi_pair: Option<(String, String)> = None;

    for conj in &on_conjuncts {
        if let Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
        } = conj
        {
            let l_refs = expr_referenced_qualifiers(left);
            let r_refs = expr_referenced_qualifiers(right);

            if l_refs.len() == 1 && r_refs.len() == 1 {
                let l_q = l_refs.iter().next().unwrap();
                let r_q = r_refs.iter().next().unwrap();

                if l_q == right_qualifier && r_q != right_qualifier {
                    if let (Expr::Column(inner_col), Expr::Column(outer_col)) =
                        (left.as_ref(), right.as_ref())
                    {
                        let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                        let out_name = outer_col.split('.').next_back().unwrap_or(outer_col);
                        equi_pair = Some((out_name.to_string(), in_name.to_string()));
                        break;
                    }
                } else if r_q == right_qualifier && l_q != right_qualifier {
                    if let (Expr::Column(outer_col), Expr::Column(inner_col)) =
                        (left.as_ref(), right.as_ref())
                    {
                        let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                        let out_name = outer_col.split('.').next_back().unwrap_or(outer_col);
                        equi_pair = Some((out_name.to_string(), in_name.to_string()));
                        break;
                    }
                }
            }
        }
    }

    // 2. Hash Join Strategy (if equi-join condition exists)
    if equi_pair.is_some() {
        let join_budget = resource_sig
            .map(|r| r.join_budget)
            .unwrap_or(DEFAULT_JOIN_MEMORY_BUDGET_BYTES);
        let hash_cost = if let Some(rg) = row_goal {
            CostModel::hash_join_row_goal_cost(
                left_cost,
                expected_probes,
                right_scan.cost,
                right_scan.estimated_rows,
                join_budget,
                rg.k() as f64,
            )
        } else if join_type == JoinType::Semi {
            CostModel::hash_semi_cost(
                left_cost,
                left_rows,
                right_scan.cost,
                right_scan.estimated_rows,
                join_budget,
            )
        } else {
            CostModel::hash_anti_cost(
                left_cost,
                left_rows,
                right_scan.cost,
                right_scan.estimated_rows,
                join_budget,
            )
        };

        if hash_cost < best_cost {
            best_cost = hash_cost;
            best_strategy = JoinStrategy::Hash;
            best_index = None;
        }
    }

    // 3. Index Existence Probe Strategy (if equi-join condition and inner table has index)
    if let Some((outer_col, inner_col)) = &equi_pair {
        if !right_table.is_cte {
            let pk_col = &right_table.schema.columns[right_table.schema.primary_key_idx].name;
            let mut available_index: Option<(bool, String)> = None;

            if pk_col == inner_col {
                available_index = Some((true, "__primary__".to_string()));
            } else if let Some(sec_idx) = right_table
                .indexes
                .iter()
                .find(|idx| right_table.schema.columns[idx.column_idx].name == *inner_col)
            {
                available_index = Some((false, sec_idx.name.clone()));
            }

            if let Some((is_pk, idx_name)) = available_index {
                let null_frac = left_table
                    .stats
                    .as_ref()
                    .and_then(|s| s.column(outer_col))
                    .map(|c| c.null_fraction())
                    .unwrap_or(0.0);

                let has_residual = right_scan.residual_filter.is_some() || on_conjuncts.len() > 1;

                let idx_cost = CostModel::index_probe_join_cost(
                    left_cost,
                    expected_probes,
                    null_frac,
                    is_pk,
                    has_residual,
                );

                if idx_cost < best_cost {
                    best_cost = idx_cost;
                    best_strategy = JoinStrategy::IndexProbe;
                    best_index = Some(idx_name);
                }
            }
        }
    }

    let (early_term, early_phase, est_examined, est_qualifying) = if let Some(rg) = row_goal {
        (
            true,
            Some("possible".to_string()),
            Some(expected_probes),
            Some(rg.k() as f64),
        )
    } else {
        (false, None, None, None)
    };

    (
        best_strategy,
        best_index,
        best_cost,
        est_examined,
        est_qualifying,
        early_term,
        early_phase,
    )
}

/// Selected physical aggregation strategy, updated cumulative cost, and output ordering property.
#[allow(clippy::too_many_arguments)]
pub fn plan_aggregation_strategy(
    input_cost: f64,
    input_rows: f64,
    input_ordering: Option<&crate::query::merge_join::OrderingProperty>,
    group_by: &[Expr],
    aggregates: &[AggregateExpr],
    table_qualifier: Option<&str>,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
) -> (
    Option<AggregateStrategy>,
    f64,
    crate::query::merge_join::OrderingProperty,
) {
    if group_by.is_empty() && aggregates.is_empty() {
        return (
            None,
            input_cost,
            input_ordering.cloned().unwrap_or_default(),
        );
    }

    let is_ordered = input_ordering
        .map(|ord| ord.satisfies_group_by(group_by, table_qualifier))
        .unwrap_or(false);

    let agg_budget = resource_sig
        .map(|r| r.agg_budget)
        .unwrap_or(DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES);
    let sort_budget = resource_sig
        .map(|r| r.sort_budget)
        .unwrap_or(DEFAULT_SORT_MEMORY_BUDGET_BYTES);

    let key = FeedbackKey::new(
        FeedbackContext::Aggregate {
            group_by: group_by
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            aggregates: aggregates
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        },
        param_sig.cloned(),
        stats_gen,
    )
    .with_resource_signature(resource_sig.cloned());

    let (spill_status, adj_rows) = if let Some(fb_reg) = feedback {
        (
            fb_reg.get_spill_status(&key),
            fb_reg.get_adjusted_cardinality(&key, input_rows),
        )
    } else {
        (SpillStatus::Unknown, input_rows)
    };

    let cost_streaming_no_sort =
        CostModel::streaming_aggregate_cost(adj_rows, group_by.len(), aggregates.len());

    let cost_sort = CostModel::sort_cost_with_budget(adj_rows, 64.0, sort_budget, spill_status);
    let cost_streaming_with_sort = cost_sort + cost_streaming_no_sort;

    let cost_hash = CostModel::aggregate_cost_with_budget(adj_rows, 64.0, agg_budget, spill_status);

    let is_hash_external = match spill_status {
        SpillStatus::Spilled => true,
        SpillStatus::NeverSpilled => false,
        SpillStatus::Unknown => (adj_rows * 64.0) > (agg_budget as f64),
    };

    // If no GROUP BY (scalar aggregation)
    if group_by.is_empty() {
        let strategy = AggregateStrategy::Streaming {
            sort_required: false,
        };
        return (
            Some(strategy),
            input_cost + cost_streaming_no_sort,
            crate::query::merge_join::OrderingProperty::default(),
        );
    }

    if is_ordered {
        if cost_streaming_no_sort <= cost_hash {
            let strategy = AggregateStrategy::Streaming {
                sort_required: false,
            };
            let out_ordering = crate::query::merge_join::OrderingProperty::new(
                group_by
                    .iter()
                    .filter_map(|expr| match expr {
                        Expr::Column(name) => Some(crate::query::merge_join::OrderingColumn {
                            column: name.clone(),
                            direction: crate::query::operator::SortDirection::Ascending,
                            nulls_first: true,
                        }),
                        _ => None,
                    })
                    .collect(),
            );
            (
                Some(strategy),
                input_cost + cost_streaming_no_sort,
                out_ordering,
            )
        } else {
            let strategy = if is_hash_external {
                AggregateStrategy::External
            } else {
                AggregateStrategy::Hash
            };
            (
                Some(strategy),
                input_cost + cost_hash,
                crate::query::merge_join::OrderingProperty::default(),
            )
        }
    } else if cost_streaming_with_sort < cost_hash {
        let strategy = AggregateStrategy::Streaming {
            sort_required: true,
        };
        let out_ordering = crate::query::merge_join::OrderingProperty::new(
            group_by
                .iter()
                .filter_map(|expr| match expr {
                    Expr::Column(name) => Some(crate::query::merge_join::OrderingColumn {
                        column: name.clone(),
                        direction: crate::query::operator::SortDirection::Ascending,
                        nulls_first: true,
                    }),
                    _ => None,
                })
                .collect(),
        );
        (
            Some(strategy),
            input_cost + cost_streaming_with_sort,
            out_ordering,
        )
    } else {
        let strategy = if is_hash_external {
            AggregateStrategy::External
        } else {
            AggregateStrategy::Hash
        };
        (
            Some(strategy),
            input_cost + cost_hash,
            crate::query::merge_join::OrderingProperty::default(),
        )
    }
}

/// Cost-based and statistics-aware query optimizer and physical access-path planner.
pub struct Optimizer;

impl Optimizer {
    /// Optimizes a bound SQL query (`BoundSelect`) into a deterministic cost-based `PhysicalPlan`.
    pub fn optimize(bound: &BoundSelect, tables: &[TableOptInfo]) -> Result<PhysicalPlan> {
        Self::optimize_with_feedback(bound, tables, None, None, 0)
    }

    /// Optimizes a bound SQL query with adaptive runtime cardinality feedback.
    pub fn optimize_with_feedback(
        bound: &BoundSelect,
        tables: &[TableOptInfo],
        feedback: Option<&FeedbackRegistry>,
        param_sig: Option<&ParamPlanningSignature>,
        stats_gen: u64,
    ) -> Result<PhysicalPlan> {
        Self::optimize_with_resource_feedback(bound, tables, feedback, param_sig, None, stats_gen)
    }

    /// Optimizes a bound SQL query using the bounded property-aware optimizer memo.
    pub fn optimize_with_memo(
        bound: &BoundSelect,
        tables: &[TableOptInfo],
        memo: &mut crate::query::memo::OptimizerMemo,
        feedback: Option<&FeedbackRegistry>,
        param_sig: Option<&ParamPlanningSignature>,
        resource_sig: Option<&ResourcePlanningSignature>,
        stats_gen: u64,
    ) -> Result<PhysicalPlan> {
        let is_multi_table = !bound.joins.is_empty();
        let where_conjuncts = match &bound.filter {
            Some(f) => split_conjunction(f),
            None => Vec::new(),
        };

        if !is_multi_table {
            let req_cols = compute_query_required_columns(bound);
            let ref_cols = collect_table_referenced_columns(&tables[0], bound, &where_conjuncts);
            let candidate_scans = plan_table_scan_candidates(
                &tables[0],
                where_conjuncts.clone(),
                Some(&ref_cols),
                false,
                feedback,
                param_sig,
                resource_sig,
                stats_gen,
                Some(&req_cols),
            );

            let log_op = crate::query::memo::LogicalOp::Scan {
                table_name: tables[0].name.clone(),
                table_alias: tables[0].alias.clone(),
                filter: bound.filter.as_ref().map(|f| f.to_string()),
            };

            let gid = match memo.get_or_create_group(log_op) {
                Ok(id) => id,
                Err(_) => {
                    return Self::optimize_with_resource_feedback(
                        bound,
                        tables,
                        feedback,
                        param_sig,
                        resource_sig,
                        stats_gen,
                    );
                }
            };

            let safe_row_goal = bound.group_by.is_empty()
                && bound.aggregates.is_empty()
                && bound.window_exprs.is_empty();
            let root_row_goal = if safe_row_goal {
                crate::query::physical_properties::RowGoal::from_limit_offset(
                    bound.limit,
                    bound.offset,
                )
            } else {
                None
            };

            for base_table_scan in &candidate_scans {
                let mut cur_cost = base_table_scan.cost;
                let mut cur_rows = base_table_scan.estimated_rows;

                let scan_ordering =
                    crate::query::physical_properties::ordering_property_for_table_scan(
                        &tables[0],
                        base_table_scan,
                    );
                let (agg_strategy, post_agg_cost, post_agg_ordering) = plan_aggregation_strategy(
                    cur_cost,
                    cur_rows,
                    Some(&scan_ordering),
                    &bound.group_by,
                    &bound.aggregates,
                    Some(tables[0].qualifier()),
                    feedback,
                    param_sig,
                    resource_sig,
                    stats_gen,
                );
                cur_cost = post_agg_cost;
                let current_ordering = post_agg_ordering;

                if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
                    cur_rows = CardinalityEstimator::estimate_group_by_cardinality(
                        cur_rows,
                        &bound.group_by,
                        tables[0].stats.as_ref(),
                        &tables[0].schema,
                    );
                }

                if let Some(having) = &bound.having {
                    cur_cost += CostModel::filter_cost(cur_rows);
                    let having_sel = CardinalityEstimator::estimate_expr_selectivity(
                        having,
                        tables[0].stats.as_ref(),
                        &tables[0].schema,
                    );
                    cur_rows = (cur_rows * having_sel)
                        .clamp(if cur_rows > 0.0 { 1.0 } else { 0.0 }, cur_rows);
                }

                let (sort_required, top_n, final_ordering) = if bound.order_by.is_empty() {
                    (false, false, current_ordering)
                } else {
                    let req_ord =
                        crate::query::physical_properties::OrderingProperty::from_sort_keys(
                            &bound.order_by,
                        );
                    let provides = current_ordering.satisfies(&req_ord);
                    if provides {
                        (false, false, current_ordering)
                    } else if let Some(rg) = root_row_goal.as_ref() {
                        let top_n_cost = CostModel::top_n_cost(cur_rows, rg.k() as usize, 64.0);
                        cur_cost += top_n_cost;
                        (false, true, req_ord)
                    } else {
                        let sort_cost = CostModel::sort_cost(cur_rows);
                        cur_cost += sort_cost;
                        (true, false, req_ord)
                    }
                };

                let plan = PhysicalPlan {
                    ctes: bound.ctes.clone(),
                    base_table: base_table_scan.clone(),
                    joins: Vec::new(),
                    post_join_filter: None,
                    group_by: bound.group_by.clone(),
                    aggregates: bound.aggregates.clone(),
                    aggregate_strategy: agg_strategy,
                    having: bound.having.clone(),
                    window_exprs: bound.window_exprs.clone(),
                    projections: bound.projections.clone(),
                    order_by: bound.order_by.clone(),
                    order_by_sort_required: sort_required,
                    order_by_top_n: top_n,
                    provided_ordering: Some(final_ordering),
                    offset: bound.offset,
                    limit: bound.limit,
                    set_op: None,
                    logical_rewrites: Vec::new(),
                    inferred_predicates: Vec::new(),
                    row_goal: base_table_scan.row_goal.or(root_row_goal),
                    estimated_rows_examined: base_table_scan.estimated_rows_examined,
                    estimated_qualifying_rows: base_table_scan.estimated_qualifying_rows,
                    row_goal_early_termination: base_table_scan.early_termination_possible,
                    total_cost: cur_cost,
                    estimated_rows: cur_rows,
                };

                let desc = format!("{:?}", base_table_scan.access_path);
                let _ = memo.add_alternative(gid, plan, desc);
            }

            let root_ordering = if bound.order_by.is_empty() {
                crate::query::physical_properties::OrderingProperty::default()
            } else {
                crate::query::physical_properties::OrderingProperty::from_sort_keys(&bound.order_by)
            };

            let prop_key = crate::query::memo::PhysicalPropertyKey::new(
                &root_ordering,
                &req_cols,
                root_row_goal.as_ref(),
                resource_sig,
            );

            if let Ok(best) = memo.optimize_group(
                gid,
                &prop_key,
                &root_ordering,
                &req_cols,
                root_row_goal.as_ref(),
            ) {
                return Ok(best.physical_plan);
            }
        }

        Self::optimize_with_resource_feedback(
            bound,
            tables,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
        )
    }

    /// Optimizes a bound SQL query with adaptive runtime cardinality and resource feedback.
    #[allow(clippy::type_complexity)]
    pub fn optimize_with_resource_feedback(
        bound: &BoundSelect,
        tables: &[TableOptInfo],
        feedback: Option<&FeedbackRegistry>,
        param_sig: Option<&ParamPlanningSignature>,
        resource_sig: Option<&ResourcePlanningSignature>,
        stats_gen: u64,
    ) -> Result<PhysicalPlan> {
        let is_multi_table = !bound.joins.is_empty();

        let where_conjuncts = match &bound.filter {
            Some(f) => split_conjunction(f),
            None => Vec::new(),
        };

        if !is_multi_table {
            let mut metrics = InferenceMetrics::default();
            let constraint_set =
                ConstraintSet::extract_from_conjuncts(&where_conjuncts, &mut metrics);
            let req_cols = compute_query_required_columns(bound);
            let ref_cols = collect_table_referenced_columns(&tables[0], bound, &where_conjuncts);
            let candidate_scans = plan_table_scan_candidates(
                &tables[0],
                where_conjuncts.clone(),
                Some(&ref_cols),
                false,
                feedback,
                param_sig,
                resource_sig,
                stats_gen,
                Some(&req_cols),
            );

            let safe_row_goal = bound.group_by.is_empty()
                && bound.aggregates.is_empty()
                && bound.window_exprs.is_empty();
            let root_row_goal = if safe_row_goal {
                crate::query::physical_properties::RowGoal::from_limit_offset(
                    bound.limit,
                    bound.offset,
                )
            } else {
                None
            };
            let table_rows = CardinalityEstimator::estimate_table_rows(tables[0].stats.as_ref());

            let mut best_plan_info: Option<(
                PhysicalTableScan,
                Option<AggregateStrategy>,
                f64,
                f64,
                bool,
                bool,
                crate::query::physical_properties::OrderingProperty,
            )> = None;

            for base_table_scan in candidate_scans {
                let mut cur_cost = base_table_scan.cost;
                let mut cur_rows = base_table_scan.estimated_rows;

                let scan_ordering =
                    crate::query::physical_properties::ordering_property_for_table_scan(
                        &tables[0],
                        &base_table_scan,
                    );
                let (agg_strategy, post_agg_cost, post_agg_ordering) = plan_aggregation_strategy(
                    cur_cost,
                    cur_rows,
                    Some(&scan_ordering),
                    &bound.group_by,
                    &bound.aggregates,
                    Some(tables[0].qualifier()),
                    feedback,
                    param_sig,
                    resource_sig,
                    stats_gen,
                );
                cur_cost = post_agg_cost;
                let mut current_ordering = post_agg_ordering;

                if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
                    cur_rows = CardinalityEstimator::estimate_group_by_cardinality(
                        cur_rows,
                        &bound.group_by,
                        tables[0].stats.as_ref(),
                        &tables[0].schema,
                    );
                }

                if let Some(having) = &bound.having {
                    cur_cost += CostModel::filter_cost(cur_rows);
                    let having_sel = CardinalityEstimator::estimate_expr_selectivity(
                        having,
                        tables[0].stats.as_ref(),
                        &tables[0].schema,
                    );
                    cur_rows = (cur_rows * having_sel)
                        .clamp(if cur_rows > 0.0 { 1.0 } else { 0.0 }, cur_rows);
                }

                if !bound.window_exprs.is_empty() {
                    cur_cost += CostModel::window_cost(cur_rows);
                    if let Some(w) = bound.window_exprs.first() {
                        let mut win_cols = Vec::new();
                        for p in &w.window_spec.partition_by {
                            if let Expr::Column(col) = p {
                                win_cols.push(crate::query::physical_properties::OrderingColumn {
                                    column: col.clone(),
                                    direction: crate::query::operator::SortDirection::Ascending,
                                    nulls_first: true,
                                });
                            }
                        }
                        for o in &w.window_spec.order_by {
                            if let Expr::Column(col) = &o.expression {
                                win_cols.push(crate::query::physical_properties::OrderingColumn {
                                    column: col.clone(),
                                    direction: o.direction,
                                    nulls_first: o.nulls_first.unwrap_or(
                                        o.direction
                                            == crate::query::operator::SortDirection::Ascending,
                                    ),
                                });
                            }
                        }
                        if !win_cols.is_empty() {
                            current_ordering =
                                crate::query::physical_properties::OrderingProperty::new(win_cols);
                        }
                    }
                }

                let mut sort_required = false;
                let mut top_n = false;
                let mut planned_scan = base_table_scan.clone();

                if let Some(rg) = &root_row_goal {
                    let k_req = rg.k() as f64;
                    let is_already_sorted = if !bound.order_by.is_empty() {
                        current_ordering.satisfies_order_by(
                            &bound.order_by,
                            Some(tables[0].qualifier()),
                            Some(&bound.projections),
                        )
                    } else {
                        true
                    };

                    if is_already_sorted {
                        // Ordered scan early termination path
                        let residual_sel = if let Some(rf) = &base_table_scan.residual_filter {
                            CardinalityEstimator::estimate_expr_selectivity(
                                rf,
                                tables[0].stats.as_ref(),
                                &tables[0].schema,
                            )
                        } else {
                            1.0
                        };

                        let num_residuals = base_table_scan
                            .residual_filter
                            .as_ref()
                            .map(|f| split_conjunction(f).len())
                            .unwrap_or(0);
                        let is_covering = matches!(
                            base_table_scan.access_path,
                            AccessPath::IndexOnlyScan { .. }
                        );
                        let is_pk = matches!(
                            base_table_scan.access_path,
                            AccessPath::PrimaryRangeScan { .. }
                                | AccessPath::PrimaryPointLookup { .. }
                                | AccessPath::FullTableScan
                        );

                        let candidate_pop = if residual_sel > 0.0 {
                            (base_table_scan.estimated_rows / residual_sel)
                                .min(table_rows)
                                .max(1.0)
                        } else {
                            table_rows
                        };

                        let mut entries_examined = CostModel::expected_entries_for_row_goal(
                            rg.k() as f64,
                            residual_sel,
                            candidate_pop,
                        );

                        let index_name = match &base_table_scan.access_path {
                            AccessPath::SecondaryIndexScan { index_name, .. }
                            | AccessPath::IndexOnlyScan { index_name, .. } => index_name.clone(),
                            AccessPath::PrimaryRangeScan { .. } | AccessPath::FullTableScan => {
                                "PRIMARY".to_string()
                            }
                            _ => String::new(),
                        };
                        if !index_name.is_empty() {
                            let filter_str = base_table_scan
                                .residual_filter
                                .as_ref()
                                .map(|f| f.to_string())
                                .unwrap_or_default();
                            let ord_fb_key = FeedbackKey::ordered_scan(
                                &tables[0].name,
                                &index_name,
                                filter_str,
                                rg.k(),
                                param_sig.cloned(),
                                stats_gen,
                            )
                            .with_resource_signature(resource_sig.cloned());
                            if let Some(fb_reg) = feedback {
                                if let Some(fb) = fb_reg.get_feedback(&ord_fb_key) {
                                    entries_examined =
                                        fb.effective_cardinality.clamp(1.0, candidate_pop);
                                }
                            }
                        }

                        let qualifying_rows = (entries_examined * residual_sel)
                            .min(k_req)
                            .min(base_table_scan.estimated_rows)
                            .max(0.0);
                        let scan_cost = CostModel::ordered_scan_row_goal_cost(
                            entries_examined,
                            is_covering,
                            is_pk,
                            num_residuals,
                        );

                        planned_scan.row_goal = Some(*rg);
                        planned_scan.estimated_rows_examined = Some(entries_examined);
                        planned_scan.estimated_qualifying_rows = Some(qualifying_rows);
                        planned_scan.early_termination_possible = true;
                        planned_scan.cost = scan_cost;
                        planned_scan.estimated_rows = qualifying_rows;

                        cur_cost = scan_cost;
                        cur_rows = qualifying_rows;
                        sort_required = false;
                        top_n = false;
                    } else {
                        // Selective index / full scan candidate + Top-N
                        sort_required = true;
                        top_n = true;
                        let top_n_cost = CostModel::top_n_cost(cur_rows, rg.k() as usize, 64.0);
                        cur_cost += top_n_cost;
                        cur_rows = cur_rows.min(k_req);
                        current_ordering =
                            crate::query::physical_properties::OrderingProperty::from_sort_keys(
                                &bound.order_by,
                            );
                    }
                } else if !bound.order_by.is_empty() {
                    let sort_budget = resource_sig
                        .map(|r| r.sort_budget)
                        .unwrap_or(DEFAULT_SORT_MEMORY_BUDGET_BYTES);
                    let key = FeedbackKey::new(
                        FeedbackContext::Sort {
                            order_by: bound
                                .order_by
                                .iter()
                                .map(|k| k.to_string())
                                .collect::<Vec<_>>()
                                .join(", "),
                        },
                        param_sig.cloned(),
                        stats_gen,
                    )
                    .with_resource_signature(resource_sig.cloned());

                    let is_already_sorted = current_ordering.satisfies_order_by(
                        &bound.order_by,
                        Some(tables[0].qualifier()),
                        Some(&bound.projections),
                    );
                    if is_already_sorted {
                        sort_required = false;
                        top_n = false;
                    } else {
                        sort_required = true;
                        let sort_cost = if let Some(fb_reg) = feedback {
                            let spill_status = fb_reg.get_spill_status(&key);
                            let adj_rows = fb_reg.get_adjusted_cardinality(&key, cur_rows);
                            CostModel::sort_cost_with_budget(
                                adj_rows,
                                64.0,
                                sort_budget,
                                spill_status,
                            )
                        } else {
                            CostModel::sort_cost_with_budget(
                                cur_rows,
                                64.0,
                                sort_budget,
                                SpillStatus::Unknown,
                            )
                        };
                        top_n = false;
                        cur_cost += sort_cost;
                        current_ordering =
                            crate::query::physical_properties::OrderingProperty::from_sort_keys(
                                &bound.order_by,
                            );
                    }
                }

                if let Some(limit_val) = bound.limit {
                    let off = bound.offset.unwrap_or(0) as f64;
                    cur_rows = ((cur_rows - off).max(0.0)).min(limit_val as f64);
                }

                let post_proj_ordering = current_ordering.project(&bound.projections);
                current_ordering = post_proj_ordering;

                match &best_plan_info {
                    None => {
                        best_plan_info = Some((
                            planned_scan,
                            agg_strategy,
                            cur_cost,
                            cur_rows,
                            sort_required,
                            top_n,
                            current_ordering,
                        ));
                    }
                    Some((_, _, best_cost, _, _, _, _)) => {
                        if cur_cost < *best_cost {
                            best_plan_info = Some((
                                planned_scan,
                                agg_strategy,
                                cur_cost,
                                cur_rows,
                                sort_required,
                                top_n,
                                current_ordering,
                            ));
                        }
                    }
                }
            }

            let (
                base_table_scan,
                agg_strategy,
                cur_cost,
                cur_rows,
                sort_required,
                top_n,
                final_ordering,
            ) = best_plan_info.unwrap_or_else(|| {
                let default_scan = PhysicalTableScan::new_with_access_path(
                    tables[0].name.clone(),
                    tables[0].alias.clone(),
                    AccessPath::FullTableScan,
                );
                (
                    default_scan,
                    None,
                    0.0,
                    0.0,
                    !bound.order_by.is_empty(),
                    false,
                    crate::query::physical_properties::OrderingProperty::default(),
                )
            });

            return Ok(PhysicalPlan {
                ctes: bound.ctes.clone(),
                base_table: base_table_scan.clone(),
                joins: Vec::new(),
                post_join_filter: None,
                group_by: bound.group_by.clone(),
                aggregates: bound.aggregates.clone(),
                aggregate_strategy: agg_strategy,
                having: bound.having.clone(),
                window_exprs: bound.window_exprs.clone(),
                projections: bound.projections.clone(),
                order_by: bound.order_by.clone(),
                order_by_sort_required: sort_required,
                order_by_top_n: top_n,
                provided_ordering: Some(final_ordering),
                offset: bound.offset,
                limit: bound.limit,
                set_op: None,
                logical_rewrites: Vec::new(),
                inferred_predicates: constraint_set.inferences,
                row_goal: base_table_scan.row_goal.or(root_row_goal),
                estimated_rows_examined: base_table_scan.estimated_rows_examined,
                estimated_qualifying_rows: base_table_scan.estimated_qualifying_rows,
                row_goal_early_termination: base_table_scan.early_termination_possible,
                total_cost: cur_cost,
                estimated_rows: cur_rows,
            });
        }

        // Multi-table query: check if all joins are INNER JOINs eligible for full reordering
        let has_any_stats = tables.iter().any(|t| t.stats.is_some());
        let all_inner = bound.joins.iter().all(|j| j.join_type == JoinType::Inner);

        if all_inner && tables.len() >= 2 && has_any_stats {
            return optimize_all_inner_joins(
                bound,
                tables,
                &where_conjuncts,
                feedback,
                param_sig,
                resource_sig,
                stats_gen,
            );
        }

        // Query with OUTER JOIN barriers or unanalyzed default sequential ordering
        optimize_with_outer_barriers(
            bound,
            tables,
            &where_conjuncts,
            feedback,
            param_sig,
            resource_sig,
            stats_gen,
        )
    }
}

/// Extracts the authoritative column demand for a bound SQL query.
pub fn compute_query_required_columns(
    bound: &BoundSelect,
) -> crate::query::physical_properties::RequiredColumns {
    use std::collections::BTreeSet;
    let mut cols = BTreeSet::new();

    // 1. Projections
    for (expr, _) in &bound.projections {
        expr.collect_referenced_columns(&mut cols);
    }

    // 2. WHERE filter
    if let Some(f) = &bound.filter {
        f.collect_referenced_columns(&mut cols);
    }

    // 3. JOIN conditions
    for join in &bound.joins {
        join.on.collect_referenced_columns(&mut cols);
    }

    // 4. GROUP BY
    for gb in &bound.group_by {
        gb.collect_referenced_columns(&mut cols);
    }

    // 5. Aggregates
    for agg in &bound.aggregates {
        agg.collect_referenced_columns(&mut cols);
    }

    // 6. HAVING
    if let Some(h) = &bound.having {
        h.collect_referenced_columns(&mut cols);
    }

    // 7. Window expressions
    for win in &bound.window_exprs {
        win.collect_referenced_columns(&mut cols);
    }

    // 8. ORDER BY
    for sk in &bound.order_by {
        sk.collect_referenced_columns(&mut cols);
    }

    if cols.contains("*") {
        return crate::query::physical_properties::RequiredColumns::all();
    }

    crate::query::physical_properties::RequiredColumns::from_columns(cols)
}

/// Candidate access path evaluated during cost-based access-path selection.
#[derive(Debug, Clone)]
struct AccessPathCandidate {
    access_path: AccessPath,
    residual_filter: Option<Expr>,
    estimated_rows: f64,
    cost: f64,
    priority: u8,
    index_id: Option<IndexId>,
    index_name: Option<String>,
    late_materialization: bool,
    index_covered_filter: Option<Expr>,
    base_record_filter: Option<Expr>,
}

/// Analyzes local predicates for a single table and selects the optimal cost-based access path.
#[allow(clippy::too_many_arguments)]
fn plan_table_scan(
    table: &TableOptInfo,
    conjuncts: Vec<Expr>,
    referenced_columns: Option<&HashSet<usize>>,
    is_multi_table: bool,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
    required_cols: Option<&crate::query::physical_properties::RequiredColumns>,
) -> PhysicalTableScan {
    let candidates = plan_table_scan_candidates(
        table,
        conjuncts,
        referenced_columns,
        is_multi_table,
        feedback,
        param_sig,
        resource_sig,
        stats_gen,
        required_cols,
    );
    candidates.into_iter().next().unwrap()
}

/// Generates all valid candidate table scans for a table relation.
#[allow(clippy::too_many_arguments)]
pub fn plan_table_scan_candidates(
    table: &TableOptInfo,
    conjuncts: Vec<Expr>,
    referenced_columns: Option<&HashSet<usize>>,
    _is_multi_table: bool,
    feedback: Option<&FeedbackRegistry>,
    param_sig: Option<&ParamPlanningSignature>,
    resource_sig: Option<&ResourcePlanningSignature>,
    stats_gen: u64,
    required_cols: Option<&crate::query::physical_properties::RequiredColumns>,
) -> Vec<PhysicalTableScan> {
    let qualifier = table.qualifier();
    // 1. Unqualify conjuncts so they match raw table schema column names
    let unqual_conjuncts: Vec<Expr> = conjuncts
        .iter()
        .map(|c| {
            let unqual = unqualify_expr(c, qualifier);
            unqualify_expr(&unqual, &table.name)
        })
        .collect();

    let mut metrics = InferenceMetrics::default();
    let constraint_set = ConstraintSet::extract_from_conjuncts(&unqual_conjuncts, &mut metrics);
    if constraint_set.is_unsatisfiable() {
        return vec![PhysicalTableScan::new_empty(
            table.name.clone(),
            table.alias.clone(),
        )];
    }

    // If this is a CTE / in-memory relation, it does not possess a physical B-Tree index or primary key path.
    if table.is_cte {
        let residual_filter = recombine_conjunction(unqual_conjuncts.clone());
        let mut estimated_rows = CardinalityEstimator::estimate_scan_cardinality(
            table.stats.as_ref(),
            residual_filter.as_ref(),
            &table.schema,
        );
        if let Some(fb_reg) = feedback {
            let filter_repr = residual_filter
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default();
            let key = FeedbackKey::scan(
                &table.name,
                "FullTableScan",
                filter_repr,
                param_sig.cloned(),
                stats_gen,
            )
            .with_resource_signature(resource_sig.cloned());
            estimated_rows = fb_reg.get_adjusted_cardinality(&key, estimated_rows);
        }
        let cost = CostModel::table_scan_cost(estimated_rows, unqual_conjuncts.len());

        let (req_names, pruned_names, _) = table.schema.columns.iter().enumerate().fold(
            (Vec::new(), Vec::new(), std::collections::BTreeSet::new()),
            |(mut req, mut pruned, mut idx), (i, c)| {
                if required_cols
                    .is_none_or(|rc| rc.is_required_for_table(qualifier, &table.name, &c.name))
                {
                    req.push(c.name.clone());
                    idx.insert(i);
                } else {
                    pruned.push(c.name.clone());
                }
                (req, pruned, idx)
            },
        );

        return vec![PhysicalTableScan {
            table_name: table.name.clone(),
            table_alias: table.alias.clone(),
            access_path: AccessPath::FullTableScan,
            residual_filter,
            estimated_rows,
            cost,
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            early_termination_possible: false,
            required_columns: req_names,
            pruned_columns: pruned_names,
            covered_columns: Vec::new(),
            late_materialization: false,
            index_covered_filter: None,
            base_record_filter: None,
        }];
    }

    let multi_index_budget = resource_sig
        .map(|r| r.multi_index_budget)
        .unwrap_or(DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES);

    let schema = &table.schema;
    let pk_col = schema.primary_key_column();
    let pk_name = &pk_col.name;
    let pk_type = pk_col.data_type;

    let (req_names, pruned_names, req_indices): (
        Vec<String>,
        Vec<String>,
        std::collections::BTreeSet<usize>,
    ) = {
        let mut req_set = std::collections::BTreeSet::new();
        if let Some(rc) = required_cols {
            if rc.is_all() {
                for col in &schema.columns {
                    req_set.insert(col.name.clone());
                }
            } else {
                for col in &schema.columns {
                    if rc.is_required_for_table(qualifier, &table.name, &col.name) {
                        req_set.insert(col.name.clone());
                    }
                }
            }
        } else if let Some(ref_cols) = referenced_columns {
            for &idx in ref_cols {
                if idx < schema.columns.len() {
                    req_set.insert(schema.columns[idx].name.clone());
                }
            }
        } else {
            for col in &schema.columns {
                req_set.insert(col.name.clone());
            }
        }

        let mut req_vec = Vec::new();
        let mut pruned_vec = Vec::new();
        let mut indices = std::collections::BTreeSet::new();
        for (i, col) in schema.columns.iter().enumerate() {
            if req_set.contains(&col.name) {
                req_vec.push(col.name.clone());
                indices.insert(i);
            } else {
                pruned_vec.push(col.name.clone());
            }
        }
        (req_vec, pruned_vec, indices)
    };

    let classify_residual = |conjuncts: &[Expr],
                             sec_meta: &crate::engine::catalog::IndexMetadata|
     -> (Vec<Expr>, Vec<Expr>) {
        let mut index_covered = Vec::new();
        let mut base_record = Vec::new();
        for conj in conjuncts {
            let mut cols = std::collections::BTreeSet::new();
            conj.collect_referenced_columns(&mut cols);
            let all_covered = !cols.is_empty()
                && cols.iter().all(|col_name| {
                    let bare = col_name.split('.').next_back().unwrap_or(col_name);
                    if let Some(pos) = schema.columns.iter().position(|c| c.name == bare) {
                        sec_meta.covers_column(pos)
                    } else {
                        false
                    }
                });
            if all_covered {
                index_covered.push(conj.clone());
            } else {
                base_record.push(conj.clone());
            }
        }
        (index_covered, base_record)
    };

    let table_rows = CardinalityEstimator::estimate_table_rows(table.stats.as_ref());
    let mut candidates: Vec<AccessPathCandidate> = Vec::new();

    // Candidate 1: Full Table Scan
    let full_residual = recombine_conjunction(unqual_conjuncts.clone());
    let full_rows = CardinalityEstimator::estimate_scan_cardinality(
        table.stats.as_ref(),
        full_residual.as_ref(),
        schema,
    );
    let full_cost = CostModel::table_scan_cost(
        CardinalityEstimator::estimate_table_rows(table.stats.as_ref()),
        unqual_conjuncts.len(),
    );
    candidates.push(AccessPathCandidate {
        access_path: AccessPath::FullTableScan,
        residual_filter: full_residual.clone(),
        estimated_rows: full_rows,
        cost: full_cost,
        priority: 3,
        index_id: None,
        index_name: None,
        late_materialization: false,
        index_covered_filter: None,
        base_record_filter: None,
    });

    // Candidate 2: Primary Key Point Lookup
    let mut pk_found = false;
    for (idx, conj) in unqual_conjuncts.iter().enumerate() {
        if let Some(val) = extract_col_equality(conj, pk_name, pk_type) {
            if !val.is_null() {
                pk_found = true;
                let mut residual = unqual_conjuncts.clone();
                residual.remove(idx);
                let residual_filter = recombine_conjunction(residual);
                let cost = CostModel::primary_point_lookup_cost();
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::PrimaryPointLookup { key: val },
                    residual_filter,
                    estimated_rows: 1.0,
                    cost,
                    priority: 0,
                    index_id: None,
                    index_name: None,
                    late_materialization: false,
                    index_covered_filter: None,
                    base_record_filter: None,
                });
            }
        }
    }
    if !pk_found {
        if let Some(ColumnConstraint::Exact(val)) = constraint_set.constraints.get(pk_name) {
            if !val.is_null() {
                let mut residual = unqual_conjuncts.clone();
                residual
                    .retain(|c| extract_col_equality(c, pk_name, pk_type).as_ref() != Some(val));
                let residual_filter = recombine_conjunction(residual);
                let cost = CostModel::primary_point_lookup_cost();
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::PrimaryPointLookup { key: val.clone() },
                    residual_filter,
                    estimated_rows: 1.0,
                    cost,
                    priority: 0,
                    index_id: None,
                    index_name: None,
                    late_materialization: false,
                    index_covered_filter: None,
                    base_record_filter: None,
                });
            }
        }
    }

    // Candidate 3: Primary Key Range Scan
    let (pk_start, pk_end, used_indices) =
        extract_col_range_bounds(&unqual_conjuncts, pk_name, pk_type);
    if pk_start.is_some() || pk_end.is_some() {
        let residual: Vec<Expr> = unqual_conjuncts
            .iter()
            .enumerate()
            .filter(|(i, _)| !used_indices.contains(i))
            .map(|(_, c)| c.clone())
            .collect();
        let residual_filter = recombine_conjunction(residual);
        let est_rows = CardinalityEstimator::estimate_pk_range_rows(
            table.stats.as_ref(),
            pk_start.as_ref(),
            pk_end.as_ref(),
        );
        let cost = CostModel::primary_range_scan_cost(est_rows);
        candidates.push(AccessPathCandidate {
            access_path: AccessPath::PrimaryRangeScan {
                start: pk_start,
                end: pk_end,
            },
            residual_filter,
            estimated_rows: est_rows,
            cost,
            priority: 1,
            index_id: None,
            index_name: None,
            late_materialization: false,
            index_covered_filter: None,
            base_record_filter: None,
        });
    }

    // Candidate 4: Secondary Indexes (including Covering Index-Only scans and Late Materialization)
    for sec_meta in &table.indexes {
        let is_covering =
            !req_indices.is_empty() && req_indices.iter().all(|&c| sec_meta.covers_column(c));
        let covered_column_names: Vec<String> = sec_meta
            .include_column_indices
            .iter()
            .map(|&i| schema.columns[i].name.clone())
            .collect();

        if sec_meta.column_indices.len() <= 1 {
            let sec_col = &schema.columns[sec_meta.column_idx];
            let col_name = &sec_col.name;
            let col_type = sec_col.data_type;

            let mut matched_predicate = false;

            // 4a. Point Lookups
            for (idx, conj) in unqual_conjuncts.iter().enumerate() {
                if let Some(val) = extract_col_equality(conj, col_name, col_type) {
                    if !val.is_null() {
                        matched_predicate = true;
                        let mut residual = unqual_conjuncts.clone();
                        residual.remove(idx);
                        let residual_filter = recombine_conjunction(residual.clone());
                        let est_rows = CardinalityEstimator::estimate_sec_point_rows(
                            table.stats.as_ref(),
                            sec_meta,
                        );

                        if is_covering {
                            let cost = CostModel::index_only_point_lookup_cost(est_rows);
                            candidates.push(AccessPathCandidate {
                                access_path: AccessPath::IndexOnlyScan {
                                    index_name: sec_meta.name.clone(),
                                    index_id: sec_meta.index_id,
                                    scan_type: SecondaryScanType::PointLookup(val),
                                    covered_columns: covered_column_names.clone(),
                                },
                                residual_filter,
                                estimated_rows: est_rows,
                                cost,
                                priority: 0,
                                index_id: Some(sec_meta.index_id),
                                index_name: Some(sec_meta.name.clone()),
                                late_materialization: false,
                                index_covered_filter: None,
                                base_record_filter: None,
                            });
                        } else {
                            let (index_cov, base_rec) = classify_residual(&residual, sec_meta);
                            let cost = CostModel::secondary_point_lookup_cost(est_rows);
                            candidates.push(AccessPathCandidate {
                                access_path: AccessPath::SecondaryIndexScan {
                                    index_name: sec_meta.name.clone(),
                                    index_id: sec_meta.index_id,
                                    scan_type: SecondaryScanType::PointLookup(val.clone()),
                                },
                                residual_filter: residual_filter.clone(),
                                estimated_rows: est_rows,
                                cost,
                                priority: 2,
                                index_id: Some(sec_meta.index_id),
                                index_name: Some(sec_meta.name.clone()),
                                late_materialization: false,
                                index_covered_filter: None,
                                base_record_filter: None,
                            });

                            if !index_cov.is_empty() {
                                let avg_row_width =
                                    CostModel::estimated_row_width(schema, &req_indices);
                                let late_cost = CostModel::late_materialization_scan_cost(
                                    cost,
                                    est_rows,
                                    est_rows,
                                    index_cov.len(),
                                    base_rec.len(),
                                    avg_row_width,
                                );
                                candidates.push(AccessPathCandidate {
                                    access_path: AccessPath::SecondaryIndexScan {
                                        index_name: sec_meta.name.clone(),
                                        index_id: sec_meta.index_id,
                                        scan_type: SecondaryScanType::PointLookup(val),
                                    },
                                    residual_filter,
                                    estimated_rows: est_rows,
                                    cost: late_cost,
                                    priority: 2,
                                    index_id: Some(sec_meta.index_id),
                                    index_name: Some(sec_meta.name.clone()),
                                    late_materialization: true,
                                    index_covered_filter: recombine_conjunction(index_cov),
                                    base_record_filter: recombine_conjunction(base_rec),
                                });
                            }
                        }
                    }
                }
            }

            // 4b. Range Scans
            let (sec_start, sec_end, used_indices) =
                extract_col_range_bounds(&unqual_conjuncts, col_name, col_type);
            if sec_start.is_some() || sec_end.is_some() {
                matched_predicate = true;
                let residual: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !used_indices.contains(i))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual.clone());
                let est_rows = CardinalityEstimator::estimate_sec_range_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    sec_start.as_ref(),
                    sec_end.as_ref(),
                );

                if is_covering {
                    let cost = CostModel::index_only_range_scan_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::IndexOnlyScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::RangeScan {
                                start: sec_start,
                                end: sec_end,
                            },
                            covered_columns: covered_column_names.clone(),
                        },
                        residual_filter,
                        estimated_rows: est_rows,
                        cost,
                        priority: 1,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });
                } else {
                    let (index_cov, base_rec) = classify_residual(&residual, sec_meta);
                    let cost = CostModel::secondary_range_scan_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::SecondaryIndexScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::RangeScan {
                                start: sec_start.clone(),
                                end: sec_end.clone(),
                            },
                        },
                        residual_filter: residual_filter.clone(),
                        estimated_rows: est_rows,
                        cost,
                        priority: 2,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });

                    if !index_cov.is_empty() {
                        let avg_row_width = CostModel::estimated_row_width(schema, &req_indices);
                        let late_cost = CostModel::late_materialization_scan_cost(
                            cost,
                            est_rows,
                            est_rows,
                            index_cov.len(),
                            base_rec.len(),
                            avg_row_width,
                        );
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::RangeScan {
                                    start: sec_start,
                                    end: sec_end,
                                },
                            },
                            residual_filter,
                            estimated_rows: est_rows,
                            cost: late_cost,
                            priority: 2,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: true,
                            index_covered_filter: recombine_conjunction(index_cov),
                            base_record_filter: recombine_conjunction(base_rec),
                        });
                    }
                }
            }

            // 4c. Full index scan
            if !matched_predicate {
                if is_covering {
                    let cost = CostModel::index_only_range_scan_cost(table_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::IndexOnlyScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::RangeScan {
                                start: None,
                                end: None,
                            },
                            covered_columns: covered_column_names.clone(),
                        },
                        residual_filter: full_residual.clone(),
                        estimated_rows: full_rows,
                        cost,
                        priority: 3,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });
                } else {
                    let (index_cov, base_rec) = classify_residual(&unqual_conjuncts, sec_meta);
                    let cost = CostModel::secondary_range_scan_cost(table_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::SecondaryIndexScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::RangeScan {
                                start: None,
                                end: None,
                            },
                        },
                        residual_filter: full_residual.clone(),
                        estimated_rows: full_rows,
                        cost,
                        priority: 4,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });

                    if !index_cov.is_empty() {
                        let avg_row_width = CostModel::estimated_row_width(schema, &req_indices);
                        let late_cost = CostModel::late_materialization_scan_cost(
                            cost,
                            full_rows,
                            full_rows,
                            index_cov.len(),
                            base_rec.len(),
                            avg_row_width,
                        );
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::RangeScan {
                                    start: None,
                                    end: None,
                                },
                            },
                            residual_filter: full_residual.clone(),
                            estimated_rows: full_rows,
                            cost: late_cost,
                            priority: 4,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: true,
                            index_covered_filter: recombine_conjunction(index_cov),
                            base_record_filter: recombine_conjunction(base_rec),
                        });
                    }
                }
            }
        } else {
            // Composite secondary index
            let mut prefix_values = Vec::new();
            let mut used_indices = HashSet::new();

            for &col_idx in &sec_meta.column_indices {
                let col = &schema.columns[col_idx];
                let mut found_eq = None;
                for (c_idx, conj) in unqual_conjuncts.iter().enumerate() {
                    if used_indices.contains(&c_idx) {
                        continue;
                    }
                    if let Some(val) = extract_col_equality(conj, &col.name, col.data_type) {
                        if !val.is_null() {
                            found_eq = Some((c_idx, val));
                            break;
                        }
                    }
                }

                if let Some((c_idx, val)) = found_eq {
                    used_indices.insert(c_idx);
                    prefix_values.push(val);
                } else {
                    // Contiguous prefix broke — stop matching further columns
                    break;
                }
            }

            let m = prefix_values.len();
            let total_cols = sec_meta.column_indices.len();

            if m == total_cols {
                // Full composite key exact point lookup
                let residual: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !used_indices.contains(i))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual.clone());
                let est_rows =
                    CardinalityEstimator::estimate_sec_point_rows(table.stats.as_ref(), sec_meta);

                if is_covering {
                    let cost = CostModel::index_only_point_lookup_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::IndexOnlyScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::CompositePointLookup(prefix_values),
                            covered_columns: covered_column_names.clone(),
                        },
                        residual_filter,
                        estimated_rows: est_rows,
                        cost,
                        priority: 0,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });
                } else {
                    let (index_cov, base_rec) = classify_residual(&residual, sec_meta);
                    let cost = CostModel::secondary_point_lookup_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::SecondaryIndexScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::CompositePointLookup(
                                prefix_values.clone(),
                            ),
                        },
                        residual_filter: residual_filter.clone(),
                        estimated_rows: est_rows,
                        cost,
                        priority: 2,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });

                    if !index_cov.is_empty() {
                        let avg_row_width = CostModel::estimated_row_width(schema, &req_indices);
                        let late_cost = CostModel::late_materialization_scan_cost(
                            cost,
                            est_rows,
                            est_rows,
                            index_cov.len(),
                            base_rec.len(),
                            avg_row_width,
                        );
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::CompositePointLookup(prefix_values),
                            },
                            residual_filter,
                            estimated_rows: est_rows,
                            cost: late_cost,
                            priority: 2,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: true,
                            index_covered_filter: recombine_conjunction(index_cov),
                            base_record_filter: recombine_conjunction(base_rec),
                        });
                    }
                }
            } else if m > 0 {
                // Proper composite prefix scan
                let residual: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !used_indices.contains(i))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual.clone());
                let est_rows = CardinalityEstimator::estimate_sec_prefix_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    m,
                );

                if is_covering {
                    let cost = CostModel::index_only_range_scan_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::IndexOnlyScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::PrefixScan(prefix_values),
                            covered_columns: covered_column_names.clone(),
                        },
                        residual_filter,
                        estimated_rows: est_rows,
                        cost,
                        priority: 1,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });
                } else {
                    let (index_cov, base_rec) = classify_residual(&residual, sec_meta);
                    let cost = CostModel::secondary_range_scan_cost(est_rows);
                    candidates.push(AccessPathCandidate {
                        access_path: AccessPath::SecondaryIndexScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::PrefixScan(prefix_values.clone()),
                        },
                        residual_filter: residual_filter.clone(),
                        estimated_rows: est_rows,
                        cost,
                        priority: 2,
                        index_id: Some(sec_meta.index_id),
                        index_name: Some(sec_meta.name.clone()),
                        late_materialization: false,
                        index_covered_filter: None,
                        base_record_filter: None,
                    });

                    if !index_cov.is_empty() {
                        let avg_row_width = CostModel::estimated_row_width(schema, &req_indices);
                        let late_cost = CostModel::late_materialization_scan_cost(
                            cost,
                            est_rows,
                            est_rows,
                            index_cov.len(),
                            base_rec.len(),
                            avg_row_width,
                        );
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::PrefixScan(prefix_values),
                            },
                            residual_filter,
                            estimated_rows: est_rows,
                            cost: late_cost,
                            priority: 2,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: true,
                            index_covered_filter: recombine_conjunction(index_cov),
                            base_record_filter: recombine_conjunction(base_rec),
                        });
                    }
                }
            } else {
                // Check for Skip Scan candidate (unconstrained leading prefix + contiguous constrained suffix)
                if total_cols >= 2 {
                    if let Some(first_constrained_idx) = (1..total_cols).find(|&p| {
                        let col_idx = sec_meta.column_indices[p];
                        let col = &schema.columns[col_idx];
                        unqual_conjuncts.iter().any(|conj| {
                            extract_col_equality(conj, &col.name, col.data_type).is_some()
                                || matches!(conj, Expr::IsNull(inner) if matches!(inner.as_ref(), Expr::Column(n) if n == &col.name))
                        })
                    }) {
                        let skipped_prefix_len = first_constrained_idx;
                        let skipped_prefix_columns: Vec<String> = sec_meta.column_indices
                            [0..skipped_prefix_len]
                            .iter()
                            .map(|&i| schema.columns[i].name.clone())
                            .collect();
                        let skipped_col_indices =
                            sec_meta.column_indices[0..skipped_prefix_len].to_vec();

                        let mut constrained_columns = Vec::new();
                        let mut constrained_col_indices = Vec::new();
                        let mut skip_used_conjuncts = HashSet::new();

                        for &col_idx in &sec_meta.column_indices[first_constrained_idx..] {
                            let col = &schema.columns[col_idx];
                            let mut found_eq = None;
                            for (c_idx, conj) in unqual_conjuncts.iter().enumerate() {
                                if skip_used_conjuncts.contains(&c_idx) {
                                    continue;
                                }
                                if let Some(val) =
                                    extract_col_equality(conj, &col.name, col.data_type)
                                {
                                    found_eq = Some((c_idx, val));
                                    break;
                                } else if let Expr::IsNull(inner) = conj {
                                    if let Expr::Column(name) = inner.as_ref() {
                                        if name == &col.name {
                                            found_eq = Some((c_idx, Value::Null));
                                            break;
                                        }
                                    }
                                }
                            }

                            if let Some((c_idx, val)) = found_eq {
                                skip_used_conjuncts.insert(c_idx);
                                constrained_columns.push((col.name.clone(), val));
                                constrained_col_indices.push(col_idx);
                            } else {
                                break;
                            }
                        }

                        if !constrained_columns.is_empty() {
                            let residual: Vec<Expr> = unqual_conjuncts
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| !skip_used_conjuncts.contains(i))
                                .map(|(_, c)| c.clone())
                                .collect();
                            let num_residual_filters = residual.len();
                            let residual_filter = recombine_conjunction(residual);

                            let total_index_entries = table
                                .stats
                                .as_ref()
                                .and_then(|s| s.index(&sec_meta.name))
                                .map(|i| i.indexed_row_count as f64)
                                .unwrap_or_else(|| {
                                    table
                                        .stats
                                        .as_ref()
                                        .map(|s| s.row_count as f64)
                                        .unwrap_or(crate::query::cardinality::DEFAULT_TABLE_ROWS)
                                });

                            let estimated_leaf_pages = table
                                .stats
                                .as_ref()
                                .and_then(|s| s.index(&sec_meta.name))
                                .map(|i| i.estimated_leaf_pages as f64)
                                .unwrap_or_else(|| (total_index_entries / 200.0).ceil().max(1.0));

                            let enumeration_cost = CostModel::index_enumeration_cost(
                                total_index_entries,
                                estimated_leaf_pages,
                            );

                            let est_distinct_prefixes =
                                CardinalityEstimator::estimate_distinct_prefixes(
                                    table.stats.as_ref(),
                                    schema,
                                    &skipped_col_indices,
                                );
                            let est_rows = CardinalityEstimator::estimate_skip_scan_rows(
                                table.stats.as_ref(),
                                schema,
                                &constrained_col_indices,
                            );

                            if is_covering {
                                let cost = CostModel::index_only_skip_scan_cost(
                                    enumeration_cost,
                                    est_distinct_prefixes,
                                    est_rows,
                                    num_residual_filters,
                                );
                                candidates.push(AccessPathCandidate {
                                    access_path: AccessPath::IndexOnlyScan {
                                        index_name: sec_meta.name.clone(),
                                        index_id: sec_meta.index_id,
                                        scan_type: SecondaryScanType::SkipScan {
                                            skipped_prefix_columns: skipped_prefix_columns.clone(),
                                            constrained_columns: constrained_columns.clone(),
                                        },
                                        covered_columns: covered_column_names.clone(),
                                    },
                                    residual_filter,
                                    estimated_rows: est_rows,
                                    cost,
                                    priority: 1,
                                    index_id: Some(sec_meta.index_id),
                                    index_name: Some(sec_meta.name.clone()),
                                    late_materialization: false,
                                    index_covered_filter: None,
                                    base_record_filter: None,
                                });
                            } else {
                                let cost = CostModel::skip_scan_cost(
                                    enumeration_cost,
                                    est_distinct_prefixes,
                                    est_rows,
                                    num_residual_filters,
                                );
                                candidates.push(AccessPathCandidate {
                                    access_path: AccessPath::SecondaryIndexScan {
                                        index_name: sec_meta.name.clone(),
                                        index_id: sec_meta.index_id,
                                        scan_type: SecondaryScanType::SkipScan {
                                            skipped_prefix_columns,
                                            constrained_columns,
                                        },
                                    },
                                    residual_filter,
                                    estimated_rows: est_rows,
                                    cost,
                                    priority: 2,
                                    index_id: Some(sec_meta.index_id),
                                    index_name: Some(sec_meta.name.clone()),
                                    late_materialization: false,
                                    index_covered_filter: None,
                                    base_record_filter: None,
                                });
                            }
                        }
                    }
                }

                if m == 0 {
                    if is_covering {
                        // Full composite index scan if covering all columns
                        let cost = CostModel::index_only_range_scan_cost(table_rows);
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::IndexOnlyScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::RangeScan {
                                    start: None,
                                    end: None,
                                },
                                covered_columns: covered_column_names.clone(),
                            },
                            residual_filter: full_residual.clone(),
                            estimated_rows: full_rows,
                            cost,
                            priority: 3,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: false,
                            index_covered_filter: None,
                            base_record_filter: None,
                        });
                    } else {
                        let (index_cov, base_rec) = classify_residual(&unqual_conjuncts, sec_meta);
                        let cost = CostModel::secondary_range_scan_cost(table_rows);
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::RangeScan {
                                    start: None,
                                    end: None,
                                },
                            },
                            residual_filter: full_residual.clone(),
                            estimated_rows: full_rows,
                            cost,
                            priority: 4,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                            late_materialization: false,
                            index_covered_filter: None,
                            base_record_filter: None,
                        });

                        if !index_cov.is_empty() {
                            let avg_row_width =
                                CostModel::estimated_row_width(schema, &req_indices);
                            let late_cost = CostModel::late_materialization_scan_cost(
                                cost,
                                full_rows,
                                full_rows,
                                index_cov.len(),
                                base_rec.len(),
                                avg_row_width,
                            );
                            candidates.push(AccessPathCandidate {
                                access_path: AccessPath::SecondaryIndexScan {
                                    index_name: sec_meta.name.clone(),
                                    index_id: sec_meta.index_id,
                                    scan_type: SecondaryScanType::RangeScan {
                                        start: None,
                                        end: None,
                                    },
                                },
                                residual_filter: full_residual.clone(),
                                estimated_rows: full_rows,
                                cost: late_cost,
                                priority: 4,
                                index_id: Some(sec_meta.index_id),
                                index_name: Some(sec_meta.name.clone()),
                                late_materialization: true,
                                index_covered_filter: recombine_conjunction(index_cov),
                                base_record_filter: recombine_conjunction(base_rec),
                            });
                        }
                    }
                }
            }
        }
    }

    // Candidate 5: Multi-Index Scans (Intersection for AND, Union for OR)

    // 5a. Extract single-index candidate atoms from unqual_conjuncts
    #[derive(Debug, Clone)]
    struct SingleIndexAtom {
        index_name: String,
        index_id: IndexId,
        scan_type: SecondaryScanType,
        used_conjunct_indices: Vec<usize>,
        estimated_rows: f64,
        cost: f64,
        selectivity: f64,
    }

    let mut atoms: Vec<SingleIndexAtom> = Vec::new();

    for sec_meta in &table.indexes {
        if sec_meta.column_indices.len() == 1 {
            let sec_col = &schema.columns[sec_meta.column_idx];
            let col_name = &sec_col.name;
            let col_type = sec_col.data_type;

            // Point lookups
            for (idx, conj) in unqual_conjuncts.iter().enumerate() {
                if let Some(val) = extract_col_equality(conj, col_name, col_type) {
                    if !val.is_null() {
                        let est_rows = CardinalityEstimator::estimate_sec_point_rows(
                            table.stats.as_ref(),
                            sec_meta,
                        );
                        let scan_cost = SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                        let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                        atoms.push(SingleIndexAtom {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::PointLookup(val),
                            used_conjunct_indices: vec![idx],
                            estimated_rows: est_rows,
                            cost: scan_cost,
                            selectivity: sel,
                        });
                    }
                }
            }

            // Range scans
            let (sec_start, sec_end, used_indices) =
                extract_col_range_bounds(&unqual_conjuncts, col_name, col_type);
            if sec_start.is_some() || sec_end.is_some() {
                let est_rows = CardinalityEstimator::estimate_sec_range_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    sec_start.as_ref(),
                    sec_end.as_ref(),
                );
                let scan_cost = SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                atoms.push(SingleIndexAtom {
                    index_name: sec_meta.name.clone(),
                    index_id: sec_meta.index_id,
                    scan_type: SecondaryScanType::RangeScan {
                        start: sec_start,
                        end: sec_end,
                    },
                    used_conjunct_indices: used_indices.into_iter().collect(),
                    estimated_rows: est_rows,
                    cost: scan_cost,
                    selectivity: sel,
                });
            }
        } else {
            // Composite index prefix/point lookups
            let mut prefix_values = Vec::new();
            let mut prefix_used = HashSet::new();
            for &col_idx in &sec_meta.column_indices {
                let col = &schema.columns[col_idx];
                let mut found_eq = false;
                for (idx, conj) in unqual_conjuncts.iter().enumerate() {
                    if prefix_used.contains(&idx) {
                        continue;
                    }
                    if let Some(val) = extract_col_equality(conj, &col.name, col.data_type) {
                        if !val.is_null() {
                            prefix_values.push(val);
                            prefix_used.insert(idx);
                            found_eq = true;
                            break;
                        }
                    }
                }
                if !found_eq {
                    break;
                }
            }

            if prefix_values.len() == sec_meta.column_indices.len() {
                let est_rows =
                    CardinalityEstimator::estimate_sec_point_rows(table.stats.as_ref(), sec_meta);
                let scan_cost = SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                atoms.push(SingleIndexAtom {
                    index_name: sec_meta.name.clone(),
                    index_id: sec_meta.index_id,
                    scan_type: SecondaryScanType::CompositePointLookup(prefix_values),
                    used_conjunct_indices: prefix_used.into_iter().collect(),
                    estimated_rows: est_rows,
                    cost: scan_cost,
                    selectivity: sel,
                });
            } else if !prefix_values.is_empty() {
                let est_rows = CardinalityEstimator::estimate_sec_prefix_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    prefix_values.len(),
                );
                let scan_cost = SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                atoms.push(SingleIndexAtom {
                    index_name: sec_meta.name.clone(),
                    index_id: sec_meta.index_id,
                    scan_type: SecondaryScanType::PrefixScan(prefix_values),
                    used_conjunct_indices: prefix_used.into_iter().collect(),
                    estimated_rows: est_rows,
                    cost: scan_cost,
                    selectivity: sel,
                });
            }
        }
    }

    // Sort candidate atoms by selectivity ascending (most selective first)
    atoms.sort_by(|a, b| a.selectivity.total_cmp(&b.selectivity));
    atoms.truncate(6); // Deterministic bounding

    // 5b. Multi-Index Intersection (AND) - Combinations of 2 and 3 indexes
    if atoms.len() >= 2 {
        // 2-index combinations
        for i in 0..atoms.len() {
            for j in (i + 1)..atoms.len() {
                let a1 = &atoms[i];
                let a2 = &atoms[j];
                if a1.index_id == a2.index_id {
                    continue;
                }
                let mut used_set: HashSet<usize> =
                    a1.used_conjunct_indices.iter().copied().collect();
                for &u in &a2.used_conjunct_indices {
                    used_set.insert(u);
                }
                let residual_conjuncts: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| !used_set.contains(idx))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual_conjuncts.clone());
                let used_conjuncts: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| used_set.contains(idx))
                    .map(|(_, c)| c.clone())
                    .collect();
                let combined_used_expr = recombine_conjunction(used_conjuncts);
                let est_rows = if let Some(comb_expr) = &combined_used_expr {
                    let sel = CardinalityEstimator::estimate_expr_selectivity(
                        comb_expr,
                        table.stats.as_ref(),
                        &table.schema,
                    );
                    (table_rows * sel).clamp(0.0, table_rows)
                } else {
                    CardinalityEstimator::estimate_intersection_cardinality(
                        table_rows,
                        &[a1.selectivity, a2.selectivity],
                    )
                };
                let total_cand_bytes = ((a1.estimated_rows + a2.estimated_rows)
                    * (RECORD_ID_IN_MEMORY_BYTES as f64))
                    as usize;
                let is_ext = total_cand_bytes > multi_index_budget;
                let exec_mode = if is_ext {
                    MultiIndexExecutionMode::External
                } else {
                    MultiIndexExecutionMode::InMemory
                };
                let cost = CostModel::multi_index_intersection_cost(
                    &[a1.cost, a2.cost],
                    &[a1.estimated_rows, a2.estimated_rows],
                    est_rows,
                    residual_conjuncts.len(),
                    is_ext,
                );
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::MultiIndexScan {
                        strategy: MultiIndexStrategy::Intersection,
                        child_scans: vec![
                            MultiIndexChildScan {
                                index_name: a1.index_name.clone(),
                                index_id: a1.index_id,
                                scan_type: a1.scan_type.clone(),
                                estimated_rows: a1.estimated_rows,
                                cost: a1.cost,
                            },
                            MultiIndexChildScan {
                                index_name: a2.index_name.clone(),
                                index_id: a2.index_id,
                                scan_type: a2.scan_type.clone(),
                                estimated_rows: a2.estimated_rows,
                                cost: a2.cost,
                            },
                        ],
                        execution_mode: exec_mode,
                    },
                    residual_filter,
                    estimated_rows: est_rows,
                    cost,
                    priority: 2,
                    index_id: None,
                    index_name: None,
                    late_materialization: false,
                    index_covered_filter: None,
                    base_record_filter: None,
                });
            }
        }

        // 3-index combinations
        if atoms.len() >= 3 && MAX_MULTI_INDEXES >= 3 {
            for i in 0..atoms.len() {
                for j in (i + 1)..atoms.len() {
                    for k in (j + 1)..atoms.len() {
                        let a1 = &atoms[i];
                        let a2 = &atoms[j];
                        let a3 = &atoms[k];
                        if a1.index_id == a2.index_id
                            || a1.index_id == a3.index_id
                            || a2.index_id == a3.index_id
                        {
                            continue;
                        }
                        let mut used_set: HashSet<usize> =
                            a1.used_conjunct_indices.iter().copied().collect();
                        for &u in &a2.used_conjunct_indices {
                            used_set.insert(u);
                        }
                        for &u in &a3.used_conjunct_indices {
                            used_set.insert(u);
                        }
                        let residual_conjuncts: Vec<Expr> = unqual_conjuncts
                            .iter()
                            .enumerate()
                            .filter(|(idx, _)| !used_set.contains(idx))
                            .map(|(_, c)| c.clone())
                            .collect();
                        let residual_filter = recombine_conjunction(residual_conjuncts.clone());
                        let used_conjuncts: Vec<Expr> = unqual_conjuncts
                            .iter()
                            .enumerate()
                            .filter(|(idx, _)| used_set.contains(idx))
                            .map(|(_, c)| c.clone())
                            .collect();
                        let combined_used_expr = recombine_conjunction(used_conjuncts);
                        let est_rows = if let Some(comb_expr) = &combined_used_expr {
                            let sel = CardinalityEstimator::estimate_expr_selectivity(
                                comb_expr,
                                table.stats.as_ref(),
                                &table.schema,
                            );
                            (table_rows * sel).clamp(0.0, table_rows)
                        } else {
                            CardinalityEstimator::estimate_intersection_cardinality(
                                table_rows,
                                &[a1.selectivity, a2.selectivity, a3.selectivity],
                            )
                        };
                        let total_cand_bytes =
                            ((a1.estimated_rows + a2.estimated_rows + a3.estimated_rows)
                                * (RECORD_ID_IN_MEMORY_BYTES as f64))
                                as usize;
                        let is_ext = total_cand_bytes > multi_index_budget;
                        let exec_mode = if is_ext {
                            MultiIndexExecutionMode::External
                        } else {
                            MultiIndexExecutionMode::InMemory
                        };
                        let cost = CostModel::multi_index_intersection_cost(
                            &[a1.cost, a2.cost, a3.cost],
                            &[a1.estimated_rows, a2.estimated_rows, a3.estimated_rows],
                            est_rows,
                            residual_conjuncts.len(),
                            is_ext,
                        );
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::MultiIndexScan {
                                strategy: MultiIndexStrategy::Intersection,
                                child_scans: vec![
                                    MultiIndexChildScan {
                                        index_name: a1.index_name.clone(),
                                        index_id: a1.index_id,
                                        scan_type: a1.scan_type.clone(),
                                        estimated_rows: a1.estimated_rows,
                                        cost: a1.cost,
                                    },
                                    MultiIndexChildScan {
                                        index_name: a2.index_name.clone(),
                                        index_id: a2.index_id,
                                        scan_type: a2.scan_type.clone(),
                                        estimated_rows: a2.estimated_rows,
                                        cost: a2.cost,
                                    },
                                    MultiIndexChildScan {
                                        index_name: a3.index_name.clone(),
                                        index_id: a3.index_id,
                                        scan_type: a3.scan_type.clone(),
                                        estimated_rows: a3.estimated_rows,
                                        cost: a3.cost,
                                    },
                                ],
                                execution_mode: exec_mode,
                            },
                            residual_filter,
                            estimated_rows: est_rows,
                            cost,
                            priority: 2,
                            index_id: None,
                            index_name: None,
                            late_materialization: false,
                            index_covered_filter: None,
                            base_record_filter: None,
                        });
                    }
                }
            }
        }
    }

    // 5c. Multi-Index Union (OR)
    for conj in &unqual_conjuncts {
        let disjuncts = split_disjunction(conj);
        if disjuncts.len() >= 2 && disjuncts.len() <= MAX_MULTI_INDEXES {
            let mut branch_scans: Vec<MultiIndexChildScan> = Vec::new();
            let mut branch_selectivities: Vec<f64> = Vec::new();
            let mut branch_costs: Vec<f64> = Vec::new();
            let mut branch_est_rows: Vec<f64> = Vec::new();
            let mut all_branches_indexed = true;

            for d in &disjuncts {
                let mut matched_branch = None;
                // Check single-column equality or range for each index
                for sec_meta in &table.indexes {
                    if sec_meta.column_indices.len() == 1 {
                        let sec_col = &schema.columns[sec_meta.column_idx];
                        if let Some(val) = extract_col_equality(d, &sec_col.name, sec_col.data_type)
                        {
                            if !val.is_null() {
                                let est_rows = CardinalityEstimator::estimate_sec_point_rows(
                                    table.stats.as_ref(),
                                    sec_meta,
                                );
                                let scan_cost =
                                    SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                                let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                                matched_branch = Some((
                                    MultiIndexChildScan {
                                        index_name: sec_meta.name.clone(),
                                        index_id: sec_meta.index_id,
                                        scan_type: SecondaryScanType::PointLookup(val),
                                        estimated_rows: est_rows,
                                        cost: scan_cost,
                                    },
                                    sel,
                                    scan_cost,
                                    est_rows,
                                ));
                                break;
                            }
                        }

                        let (sec_start, sec_end, _) = extract_col_range_bounds(
                            std::slice::from_ref(d),
                            &sec_col.name,
                            sec_col.data_type,
                        );
                        if sec_start.is_some() || sec_end.is_some() {
                            let est_rows = CardinalityEstimator::estimate_sec_range_rows(
                                table.stats.as_ref(),
                                sec_meta,
                                sec_start.as_ref(),
                                sec_end.as_ref(),
                            );
                            let scan_cost =
                                SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                            let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                            matched_branch = Some((
                                MultiIndexChildScan {
                                    index_name: sec_meta.name.clone(),
                                    index_id: sec_meta.index_id,
                                    scan_type: SecondaryScanType::RangeScan {
                                        start: sec_start,
                                        end: sec_end,
                                    },
                                    estimated_rows: est_rows,
                                    cost: scan_cost,
                                },
                                sel,
                                scan_cost,
                                est_rows,
                            ));
                            break;
                        }
                    } else {
                        // Composite index leading column match
                        let lead_col = &schema.columns[sec_meta.column_indices[0]];
                        if let Some(val) =
                            extract_col_equality(d, &lead_col.name, lead_col.data_type)
                        {
                            if !val.is_null() {
                                let est_rows = CardinalityEstimator::estimate_sec_prefix_rows(
                                    table.stats.as_ref(),
                                    sec_meta,
                                    1,
                                );
                                let scan_cost =
                                    SEC_INDEX_LOOKUP_COST + (SEC_INDEX_FETCH_COST * est_rows);
                                let sel = (est_rows / table_rows.max(1.0)).clamp(0.0, 1.0);
                                matched_branch = Some((
                                    MultiIndexChildScan {
                                        index_name: sec_meta.name.clone(),
                                        index_id: sec_meta.index_id,
                                        scan_type: SecondaryScanType::PrefixScan(vec![val]),
                                        estimated_rows: est_rows,
                                        cost: scan_cost,
                                    },
                                    sel,
                                    scan_cost,
                                    est_rows,
                                ));
                                break;
                            }
                        }
                    }
                }

                if let Some((child, sel, cost, est_rows)) = matched_branch {
                    branch_scans.push(child);
                    branch_selectivities.push(sel);
                    branch_costs.push(cost);
                    branch_est_rows.push(est_rows);
                } else {
                    all_branches_indexed = false;
                    break;
                }
            }

            if all_branches_indexed && branch_scans.len() == disjuncts.len() {
                // Keep the full conjunct list in residual filter to ensure 100% 3VL / SQL semantics safety
                let residual_filter = recombine_conjunction(unqual_conjuncts.clone());
                let est_rows = CardinalityEstimator::estimate_union_cardinality(
                    table_rows,
                    &branch_selectivities,
                );
                let total_cand_bytes = (branch_est_rows.iter().sum::<f64>()
                    * (RECORD_ID_IN_MEMORY_BYTES as f64))
                    as usize;
                let is_ext = total_cand_bytes > multi_index_budget;
                let exec_mode = if is_ext {
                    MultiIndexExecutionMode::External
                } else {
                    MultiIndexExecutionMode::InMemory
                };
                let cost = CostModel::multi_index_union_cost(
                    &branch_costs,
                    &branch_est_rows,
                    est_rows,
                    unqual_conjuncts.len(),
                    is_ext,
                );
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::MultiIndexScan {
                        strategy: MultiIndexStrategy::Union,
                        child_scans: branch_scans,
                        execution_mode: exec_mode,
                    },
                    residual_filter,
                    estimated_rows: est_rows,
                    cost,
                    priority: 2,
                    index_id: None,
                    index_name: None,
                    late_materialization: false,
                    index_covered_filter: None,
                    base_record_filter: None,
                });
            }
        }
    }

    if let Some(fb_reg) = feedback {
        for cand in &mut candidates {
            let filter_repr = cand
                .residual_filter
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default();
            let access_repr = match &cand.access_path {
                AccessPath::FullTableScan => "FullTableScan".to_string(),
                AccessPath::PrimaryPointLookup { .. } => "PrimaryPointLookup".to_string(),
                AccessPath::PrimaryRangeScan { .. } => "PrimaryRangeScan".to_string(),
                AccessPath::SecondaryIndexScan { index_name, .. } => {
                    format!("SecondaryIndexScan({index_name})")
                }
                AccessPath::IndexOnlyScan { index_name, .. } => {
                    format!("IndexOnlyScan({index_name})")
                }
                AccessPath::EmptyResult => "EmptyResult".to_string(),
                AccessPath::MultiIndexScan {
                    strategy,
                    child_scans,
                    ..
                } => {
                    let s = child_scans
                        .iter()
                        .map(|c| c.index_name.clone())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("MultiIndexScan({strategy},{s})")
                }
            };
            let key = FeedbackKey::scan(
                &table.name,
                access_repr,
                filter_repr,
                param_sig.cloned(),
                stats_gen,
            )
            .with_resource_signature(resource_sig.cloned());
            if fb_reg.get_feedback(&key).is_some() {
                let adj_rows = fb_reg.get_adjusted_cardinality(&key, cand.estimated_rows);
                cand.estimated_rows = adj_rows;
                match &mut cand.access_path {
                    AccessPath::EmptyResult => {
                        cand.cost = 0.0;
                    }
                    AccessPath::FullTableScan => {
                        cand.cost = CostModel::table_scan_cost(table_rows, unqual_conjuncts.len());
                    }
                    AccessPath::PrimaryPointLookup { .. } => {
                        cand.cost = CostModel::primary_point_lookup_cost();
                    }
                    AccessPath::PrimaryRangeScan { .. } => {
                        cand.cost = CostModel::primary_range_scan_cost(cand.estimated_rows);
                    }
                    AccessPath::SecondaryIndexScan { .. } => {
                        cand.cost = CostModel::secondary_range_scan_cost(cand.estimated_rows);
                    }
                    AccessPath::IndexOnlyScan { .. } => {
                        cand.cost = CostModel::index_only_range_scan_cost(cand.estimated_rows);
                    }
                    AccessPath::MultiIndexScan {
                        strategy,
                        child_scans,
                        execution_mode,
                    } => {
                        let spill_status = fb_reg.get_spill_status(&key);
                        let total_cand_rows: f64 =
                            child_scans.iter().map(|c| c.estimated_rows).sum();
                        let total_cand_bytes = total_cand_rows * (RECORD_ID_IN_MEMORY_BYTES as f64);
                        let adj_peak_mem = fb_reg.get_adjusted_peak_memory(&key, total_cand_bytes);
                        let should_spill = match spill_status {
                            SpillStatus::Spilled => true,
                            SpillStatus::NeverSpilled => false,
                            SpillStatus::Unknown => adj_peak_mem > multi_index_budget as f64,
                        };
                        *execution_mode = if should_spill {
                            MultiIndexExecutionMode::External
                        } else {
                            MultiIndexExecutionMode::InMemory
                        };
                        cand.cost = match strategy {
                            MultiIndexStrategy::Intersection => {
                                CostModel::multi_index_intersection_cost(
                                    &child_scans.iter().map(|c| c.cost).collect::<Vec<_>>(),
                                    &child_scans
                                        .iter()
                                        .map(|c| c.estimated_rows)
                                        .collect::<Vec<_>>(),
                                    cand.estimated_rows,
                                    cand.residual_filter
                                        .as_ref()
                                        .map(|f| split_conjunction(f).len())
                                        .unwrap_or(0),
                                    should_spill,
                                )
                            }
                            MultiIndexStrategy::Union => CostModel::multi_index_union_cost(
                                &child_scans.iter().map(|c| c.cost).collect::<Vec<_>>(),
                                &child_scans
                                    .iter()
                                    .map(|c| c.estimated_rows)
                                    .collect::<Vec<_>>(),
                                cand.estimated_rows,
                                cand.residual_filter
                                    .as_ref()
                                    .map(|f| split_conjunction(f).len())
                                    .unwrap_or(0),
                                should_spill,
                            ),
                        };
                    }
                };
            }
        }
    }

    // Deterministic Candidate Selection
    candidates.sort_by(|a, b| {
        const EPSILON: f64 = 1e-9;
        if (a.cost - b.cost).abs() > EPSILON {
            a.cost.total_cmp(&b.cost)
        } else if (a.estimated_rows - b.estimated_rows).abs() > EPSILON {
            a.estimated_rows.total_cmp(&b.estimated_rows)
        } else if a.priority != b.priority {
            a.priority.cmp(&b.priority)
        } else if a.index_id != b.index_id {
            a.index_id.cmp(&b.index_id)
        } else {
            a.index_name.cmp(&b.index_name)
        }
    });

    candidates
        .into_iter()
        .map(|cand| {
            let covered_cols = match &cand.access_path {
                AccessPath::IndexOnlyScan {
                    covered_columns, ..
                } => covered_columns.clone(),
                AccessPath::SecondaryIndexScan { index_name, .. } => table
                    .indexes
                    .iter()
                    .find(|m| &m.name == index_name)
                    .map(|m| {
                        m.all_column_indices()
                            .iter()
                            .map(|&i| schema.columns[i].name.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            PhysicalTableScan {
                table_name: table.name.clone(),
                table_alias: table.alias.clone(),
                access_path: cand.access_path,
                residual_filter: cand.residual_filter,
                estimated_rows: cand.estimated_rows,
                cost: cand.cost,
                row_goal: None,
                estimated_rows_examined: None,
                estimated_qualifying_rows: None,
                early_termination_possible: false,
                required_columns: req_names.clone(),
                pruned_columns: pruned_names.clone(),
                covered_columns: covered_cols,
                late_materialization: cand.late_materialization,
                index_covered_filter: cand.index_covered_filter,
                base_record_filter: cand.base_record_filter,
            }
        })
        .collect()
}

/// Collects all column indices in `table`'s schema that are referenced anywhere across the query.
pub fn collect_table_referenced_columns(
    table: &TableOptInfo,
    bound: &BoundSelect,
    where_conjuncts: &[Expr],
) -> HashSet<usize> {
    let mut cols = HashSet::new();
    let qual = table.qualifier();
    let tbl_name = &table.name;

    let mut check_column = |col_str: &str| {
        if col_str == "*" {
            for i in 0..table.schema.columns.len() {
                cols.insert(i);
            }
            return;
        }

        let (target_qual, raw_col) = if let Some(dot) = col_str.find('.') {
            (&col_str[..dot], &col_str[dot + 1..])
        } else {
            ("", col_str)
        };

        if raw_col == "*" {
            if target_qual.is_empty() || target_qual == qual || target_qual == tbl_name {
                for i in 0..table.schema.columns.len() {
                    cols.insert(i);
                }
            }
            return;
        }

        if target_qual.is_empty() || target_qual == qual || target_qual == tbl_name {
            if let Some(pos) = table.schema.columns.iter().position(|c| c.name == raw_col) {
                cols.insert(pos);
            }
        }
    };

    // 1. Projections
    for proj in &bound.projections {
        collect_columns_from_expr(&proj.0, &mut check_column);
    }

    // 2. WHERE conjuncts
    for conj in where_conjuncts {
        collect_columns_from_expr(conj, &mut check_column);
    }

    // 3. JOIN ON clauses
    for join in &bound.joins {
        collect_columns_from_expr(&join.on, &mut check_column);
    }

    // 4. GROUP BY
    for gb in &bound.group_by {
        collect_columns_from_expr(gb, &mut check_column);
    }

    // 5. Aggregates
    for agg in &bound.aggregates {
        match agg {
            AggregateExpr::CountAll => {}
            AggregateExpr::Function { expr, .. } => {
                collect_columns_from_expr(expr, &mut check_column);
            }
        }
    }

    // 6. HAVING
    if let Some(h) = &bound.having {
        collect_columns_from_expr(h, &mut check_column);
    }

    // 7. Window expressions
    for win in &bound.window_exprs {
        for arg in &win.args {
            collect_columns_from_expr(arg, &mut check_column);
        }
        if let Some(def) = &win.default {
            collect_columns_from_expr(def, &mut check_column);
        }
        for pb in &win.window_spec.partition_by {
            collect_columns_from_expr(pb, &mut check_column);
        }
        for ob in &win.window_spec.order_by {
            collect_columns_from_expr(&ob.expression, &mut check_column);
        }
    }

    // 8. ORDER BY
    for ob in &bound.order_by {
        collect_columns_from_expr(&ob.expression, &mut check_column);
    }

    cols
}

fn collect_columns_from_expr(expr: &Expr, cb: &mut impl FnMut(&str)) {
    match expr {
        Expr::Column(name) => cb(name),
        Expr::OuterColumn { name, .. } => cb(name),
        Expr::Binary { left, right, .. } => {
            collect_columns_from_expr(left, cb);
            collect_columns_from_expr(right, cb);
        }
        Expr::Unary { expr, .. } | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            collect_columns_from_expr(expr, cb);
        }
        Expr::InList { expr, list, .. } => {
            collect_columns_from_expr(expr, cb);
            for item in list {
                collect_columns_from_expr(item, cb);
            }
        }
        Expr::Subquery(sub) => {
            for col in &sub.outer_references {
                cb(col);
            }
        }
        Expr::Literal(_) | Expr::Parameter { .. } => {}
    }
}

/// Recursively splits an expression on top-level logical `AND` operators.
pub fn split_conjunction(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            let mut res = split_conjunction(left);
            res.extend(split_conjunction(right));
            res
        }
        other => vec![other.clone()],
    }
}

/// Recombines a list of conjunct expressions with logical `AND`.
pub fn recombine_conjunction(mut conjuncts: Vec<Expr>) -> Option<Expr> {
    if conjuncts.is_empty() {
        return None;
    }
    let mut acc = conjuncts.remove(0);
    for c in conjuncts {
        acc = Expr::Binary {
            left: Box::new(acc),
            op: BinaryOp::And,
            right: Box::new(c),
        };
    }
    Some(acc)
}

/// Extracts all table qualifiers referenced in column identifiers within an expression.
pub fn expr_referenced_qualifiers(expr: &Expr) -> HashSet<String> {
    let mut set = HashSet::new();
    collect_qualifiers(expr, &mut set);
    set
}

fn collect_qualifiers(expr: &Expr, set: &mut HashSet<String>) {
    match expr {
        Expr::Column(name) => {
            if let Some(dot_idx) = name.find('.') {
                set.insert(name[..dot_idx].to_string());
            }
        }
        Expr::OuterColumn { name, .. } => {
            if let Some(dot_idx) = name.find('.') {
                set.insert(name[..dot_idx].to_string());
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_qualifiers(left, set);
            collect_qualifiers(right, set);
        }
        Expr::Unary { expr, .. } | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            collect_qualifiers(expr, set);
        }
        Expr::InList { expr, list, .. } => {
            collect_qualifiers(expr, set);
            for item in list {
                collect_qualifiers(item, set);
            }
        }
        Expr::Subquery(_) | Expr::Literal(_) | Expr::Parameter { .. } => {}
    }
}

/// Strips the table qualifier prefix `"{qualifier}."` from all column identifiers in an expression.
pub fn unqualify_expr(expr: &Expr, qualifier: &str) -> Expr {
    let prefix = format!("{qualifier}.");
    match expr {
        Expr::Column(name) => {
            if let Some(stripped) = name.strip_prefix(&prefix) {
                Expr::Column(stripped.to_string())
            } else {
                Expr::Column(name.clone())
            }
        }
        Expr::OuterColumn {
            name,
            data_type,
            scope_depth,
        } => Expr::OuterColumn {
            name: name.clone(),
            data_type: *data_type,
            scope_depth: *scope_depth,
        },
        Expr::Binary { left, op, right } => Expr::Binary {
            left: Box::new(unqualify_expr(left, qualifier)),
            op: *op,
            right: Box::new(unqualify_expr(right, qualifier)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(unqualify_expr(expr, qualifier)),
        },
        Expr::IsNull(e) => Expr::IsNull(Box::new(unqualify_expr(e, qualifier))),
        Expr::IsNotNull(e) => Expr::IsNotNull(Box::new(unqualify_expr(e, qualifier))),
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(unqualify_expr(expr, qualifier)),
            list: list
                .iter()
                .map(|item| unqualify_expr(item, qualifier))
                .collect(),
            negated: *negated,
        },
        Expr::Subquery(sub) => Expr::Subquery(sub.clone()),
        Expr::Literal(val) => Expr::Literal(val.clone()),
        Expr::Parameter { index, value } => Expr::Parameter {
            index: *index,
            value: value.clone(),
        },
    }
}

/// Checks if an expression is an equality check between a specific column and a compatible literal value.
fn extract_col_equality(expr: &Expr, target_col: &str, target_type: DataType) -> Option<Value> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column(name), Expr::Literal(val))
            | (Expr::Column(name), Expr::Parameter { value: val, .. })
                if name == target_col =>
            {
                if val.data_type() == target_type || val.is_null() {
                    Some(val.clone())
                } else {
                    None
                }
            }
            (Expr::Literal(val), Expr::Column(name))
            | (Expr::Parameter { value: val, .. }, Expr::Column(name))
                if name == target_col =>
            {
                if val.data_type() == target_type || val.is_null() {
                    Some(val.clone())
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extracts range bounds `[start, end)` on a target column from a list of conjuncts.
fn extract_col_range_bounds(
    conjuncts: &[Expr],
    target_col: &str,
    target_type: DataType,
) -> (Option<Value>, Option<Value>, HashSet<usize>) {
    let mut start: Option<Value> = None;
    let mut end: Option<Value> = None;
    let mut used = HashSet::new();

    for (idx, conj) in conjuncts.iter().enumerate() {
        match conj {
            Expr::Binary {
                left,
                op: BinaryOp::GtEq,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (Expr::Column(name), Expr::Literal(val))
                | (Expr::Column(name), Expr::Parameter { value: val, .. })
                    if name == target_col && val.data_type() == target_type && !val.is_null() =>
                {
                    if start.is_none() {
                        start = Some(val.clone());
                        used.insert(idx);
                    }
                }
                (Expr::Literal(val), Expr::Column(name))
                | (Expr::Parameter { value: val, .. }, Expr::Column(name))
                    if name == target_col && val.data_type() == target_type && !val.is_null() =>
                {
                    // val >= col -> col <= val
                    // Handled as residual
                }
                _ => {}
            },
            Expr::Binary {
                left,
                op: BinaryOp::Gt,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (Expr::Column(name), Expr::Literal(val))
                | (Expr::Column(name), Expr::Parameter { value: val, .. })
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && start.is_none() =>
                {
                    start = Some(val.clone());
                    // Keep in residual filter to check strict inequality
                }
                _ => {}
            },
            Expr::Binary {
                left,
                op: BinaryOp::Lt,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (Expr::Column(name), Expr::Literal(val))
                | (Expr::Column(name), Expr::Parameter { value: val, .. })
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && end.is_none() =>
                {
                    end = Some(val.clone());
                    used.insert(idx);
                }
                (Expr::Literal(val), Expr::Column(name))
                | (Expr::Parameter { value: val, .. }, Expr::Column(name))
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && start.is_none() =>
                {
                    // val < col -> col > val
                    start = Some(val.clone());
                    // Keep in residual filter to check strict inequality
                }
                _ => {}
            },
            Expr::Binary {
                left,
                op: BinaryOp::LtEq,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (Expr::Literal(val), Expr::Column(name))
                | (Expr::Parameter { value: val, .. }, Expr::Column(name))
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && start.is_none() =>
                {
                    // val <= col -> col >= val
                    start = Some(val.clone());
                    used.insert(idx);
                }
                (Expr::Column(name), Expr::Literal(_))
                | (Expr::Column(name), Expr::Parameter { .. })
                    if name == target_col =>
                {
                    // col <= val: B+ Tree scan is exclusive on end, so keep in residual filter
                }
                _ => {}
            },
            _ => {}
        }
    }

    (start, end, used)
}

/// Splits a boolean expression joined by OR operators into a flat vector of disjunct expressions.
pub fn split_disjunction(expr: &Expr) -> Vec<Expr> {
    let mut disjuncts = Vec::new();
    fn collect(e: &Expr, acc: &mut Vec<Expr>) {
        match e {
            Expr::Binary {
                left,
                op: BinaryOp::Or,
                right,
            } => {
                collect(left, acc);
                collect(right, acc);
            }
            other => acc.push(other.clone()),
        }
    }
    collect(expr, &mut disjuncts);
    disjuncts
}

/// Recombines a vector of disjunct expressions into a single boolean OR expression tree.
pub fn recombine_disjunction(mut disjuncts: Vec<Expr>) -> Option<Expr> {
    if disjuncts.is_empty() {
        None
    } else {
        let mut expr = disjuncts.remove(0);
        for d in disjuncts {
            expr = Expr::Binary {
                left: Box::new(expr),
                op: BinaryOp::Or,
                right: Box::new(d),
            };
        }
        Some(expr)
    }
}
