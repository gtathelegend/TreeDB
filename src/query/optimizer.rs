use std::collections::HashSet;
use std::fmt;

use crate::engine::catalog::{IndexId, IndexMetadata};
use crate::engine::types::{DataType, Schema, Value};
use crate::error::Result;
use crate::query::cardinality::CardinalityEstimator;
use crate::query::cost::CostModel;
use crate::query::expr::{AggregateExpr, BinaryOp, Expr};
use crate::query::operator::SortKey;
use crate::query::stats::TableStatistics;
use crate::sql::ast::JoinType;
use crate::sql::binder::BoundSelect;

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
    /// Full sequential scan over all tuples.
    FullTableScan,
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
            },
            Self::FullTableScan => write!(f, "FullTableScan"),
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
}

impl PhysicalTableScan {
    /// Returns the active qualifier (alias if present, else table name).
    pub fn qualifier(&self) -> &str {
        self.table_alias.as_deref().unwrap_or(&self.table_name)
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
}

impl fmt::Display for JoinStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedLoop => write!(f, "NestedLoop"),
            Self::Hash => write!(f, "Hash"),
            Self::IndexProbe => write!(f, "IndexProbe"),
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
    /// Post-aggregation filter predicate (HAVING).
    pub having: Option<Expr>,
    /// Window expressions to compute.
    pub window_exprs: Vec<crate::query::expr::WindowExpr>,
    /// Output projection expressions with optional aliases.
    pub projections: Vec<(Expr, Option<String>)>,
    /// Sort keys (ORDER BY).
    pub order_by: Vec<SortKey>,
    /// Offset pagination.
    pub offset: Option<usize>,
    /// Limit pagination.
    pub limit: Option<usize>,
    /// Optional physical set operation.
    pub set_op: Option<PhysicalSetOp>,
    /// Logical transformations applied during the query rewrite phase.
    pub logical_rewrites: Vec<String>,
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
            },
            joins: Vec::new(),
            post_join_filter: None,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            having: None,
            window_exprs: Vec::new(),
            projections: Vec::new(),
            order_by: Vec::new(),
            offset: None,
            limit: None,
            set_op: Some(set_op),
            logical_rewrites: Vec::new(),
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

        if !self.ctes.is_empty() {
            for cte in &self.ctes {
                write_indent(f, indent)?;
                match &cte.kind {
                    crate::sql::binder::BoundCteKind::NonRecursive { plan } => {
                        writeln!(f, "CTE {}", cte.name)?;
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

        if self.limit.is_some() || self.offset.is_some() {
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

        if !self.order_by.is_empty() {
            write_indent(f, indent)?;
            let keys: Vec<String> = self
                .order_by
                .iter()
                .map(|k| format!("{} {:?}", k.expression, k.direction))
                .collect();
            writeln!(f, "Sort [{}]", keys.join(", "))?;
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
            writeln!(
                f,
                "Aggregate (group_by: [{}], aggs: [{}])",
                groups.join(", "),
                aggs.join(", ")
            )?;
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
fn optimize_inner_join_region_dp(graph: &JoinGraph) -> DPPlan {
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

                let join_cost = CostModel::nested_loop_join_cost(
                    left_plan.cost,
                    left_plan.rows,
                    right_scan.cost,
                    right_scan.estimated_rows,
                );

                let base_idx = left_plan.perm[0];
                let join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
                    left_plan.rows,
                    graph.relations[base_idx].table_info.stats.as_ref(),
                    right_scan.estimated_rows,
                    graph.relations[r].table_info.stats.as_ref(),
                    &on_expr,
                    &graph.relations[r].table_info.schema,
                );

                let mut cand_joins = left_plan.joins.clone();
                cand_joins.push(PhysicalJoin {
                    join_type: JoinType::Inner,
                    right: right_scan.clone(),
                    on: on_expr,
                    estimated_rows: join_rows,
                    cost: join_cost,
                    strategy: JoinStrategy::NestedLoop,
                    index_name: None,
                });

                let mut cand_perm = left_plan.perm.clone();
                cand_perm.push(r);

                let cand_plan = DPPlan {
                    perm: cand_perm,
                    cost: join_cost,
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
fn optimize_inner_join_region_greedy(graph: &JoinGraph) -> DPPlan {
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
            let join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
                cur_rows,
                graph.relations[start_idx].table_info.stats.as_ref(),
                cand_scan.estimated_rows,
                graph.relations[cand].table_info.stats.as_ref(),
                &on_expr,
                &graph.relations[cand].table_info.schema,
            );

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
        joins.push(PhysicalJoin {
            join_type: JoinType::Inner,
            right: right_scan,
            on: on_expr,
            estimated_rows: chosen.join_rows,
            cost: chosen.join_cost,
            strategy: JoinStrategy::NestedLoop,
            index_name: None,
        });

        cur_cost = chosen.join_cost;
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
fn build_join_graph(
    region_rel_indices: &[usize],
    tables: &[TableOptInfo],
    candidate_conjuncts: &[Expr],
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

    let mut relations = Vec::with_capacity(num_rels);
    for (local_id, &global_idx) in region_rel_indices.iter().enumerate() {
        let t = &tables[global_idx];
        let base_scan = plan_table_scan(t, table_local_conjuncts[local_id].clone(), true);
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
) -> Result<PhysicalPlan> {
    let num_tables = tables.len();
    let mut all_conjuncts = where_conjuncts.to_vec();
    for j in &bound.joins {
        all_conjuncts.extend(split_conjunction(&j.on));
    }

    let region_indices: Vec<usize> = (0..num_tables).collect();
    let (graph, _table_locals, unhandled) =
        build_join_graph(&region_indices, tables, &all_conjuncts);

    let best_plan = if num_tables <= MAX_ENUMERATED_RELATIONS {
        optimize_inner_join_region_dp(&graph)
    } else {
        optimize_inner_join_region_greedy(&graph)
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

    if !bound.order_by.is_empty() {
        cur_cost += CostModel::sort_cost(cur_rows);
    }
    if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
        cur_cost += CostModel::aggregate_cost(cur_rows);
    }
    if !bound.window_exprs.is_empty() {
        cur_cost += CostModel::window_cost(cur_rows);
    }

    Ok(PhysicalPlan {
        ctes: bound.ctes.clone(),
        base_table: best_plan.base_scan,
        joins: best_plan.joins,
        post_join_filter,
        group_by: bound.group_by.clone(),
        aggregates: bound.aggregates.clone(),
        having: bound.having.clone(),
        window_exprs: bound.window_exprs.clone(),
        projections: bound.projections.clone(),
        order_by: bound.order_by.clone(),
        offset: bound.offset,
        limit: bound.limit,
        set_op: None,
        logical_rewrites: Vec::new(),
        total_cost: cur_cost,
        estimated_rows: cur_rows,
    })
}

/// Optimizes a bound query with OUTER JOIN barriers, preserving outer join boundaries.
fn optimize_with_outer_barriers(
    bound: &BoundSelect,
    tables: &[TableOptInfo],
    where_conjuncts: &[Expr],
) -> Result<PhysicalPlan> {
    let is_multi_table = !bound.joins.is_empty();
    let mut table_local_conjuncts: Vec<Vec<Expr>> = vec![Vec::new(); tables.len()];
    let mut post_join_conjuncts: Vec<Expr> = Vec::new();

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
    let base_table_scan =
        plan_table_scan(&tables[0], table_local_conjuncts[0].clone(), is_multi_table);

    let mut physical_joins = Vec::new();
    let mut cur_cost = base_table_scan.cost;
    let mut cur_rows = base_table_scan.estimated_rows;

    for (join_idx, (join_type, on_expr)) in optimized_joins.into_iter().enumerate() {
        let right_table_idx = join_idx + 1;
        let right_table_scan = plan_table_scan(
            &tables[right_table_idx],
            table_local_conjuncts[right_table_idx].clone(),
            is_multi_table,
        );

        let join_cost = CostModel::nested_loop_join_cost(
            cur_cost,
            cur_rows,
            right_table_scan.cost,
            right_table_scan.estimated_rows,
        );
        let join_rows = CardinalityEstimator::estimate_join_cardinality(
            join_type,
            cur_rows,
            tables[0].stats.as_ref(),
            right_table_scan.estimated_rows,
            tables[right_table_idx].stats.as_ref(),
            &on_expr,
            &tables[right_table_idx].schema,
        );

        let (chosen_strategy, chosen_index, chosen_cost) =
            if join_type == JoinType::Semi || join_type == JoinType::Anti {
                plan_semi_anti_strategy(
                    cur_cost,
                    cur_rows,
                    &tables[0],
                    &tables[right_table_idx],
                    &right_table_scan,
                    &on_expr,
                    join_type,
                )
            } else {
                (JoinStrategy::NestedLoop, None, join_cost)
            };

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
        });
    }

    let post_join_filter = recombine_conjunction(post_join_conjuncts);
    if let Some(post_f) = &post_join_filter {
        cur_cost += CostModel::filter_cost(cur_rows);
        let sel = CardinalityEstimator::estimate_expr_selectivity(post_f, None, &tables[0].schema);
        cur_rows = (cur_rows * sel).max(if cur_rows > 0.0 { 1.0 } else { 0.0 });
    }

    if !bound.order_by.is_empty() {
        cur_cost += CostModel::sort_cost(cur_rows);
    }
    if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
        cur_cost += CostModel::aggregate_cost(cur_rows);
    }
    if !bound.window_exprs.is_empty() {
        cur_cost += CostModel::window_cost(cur_rows);
    }

    Ok(PhysicalPlan {
        ctes: bound.ctes.clone(),
        base_table: base_table_scan,
        joins: physical_joins,
        post_join_filter,
        group_by: bound.group_by.clone(),
        aggregates: bound.aggregates.clone(),
        having: bound.having.clone(),
        window_exprs: bound.window_exprs.clone(),
        projections: bound.projections.clone(),
        order_by: bound.order_by.clone(),
        offset: bound.offset,
        limit: bound.limit,
        set_op: None,
        logical_rewrites: Vec::new(),
        total_cost: cur_cost,
        estimated_rows: cur_rows,
    })
}

/// Evaluates costs of applicable physical strategies for SEMI/ANTI joins and selects the cheapest.
fn plan_semi_anti_strategy(
    left_cost: f64,
    left_rows: f64,
    left_table: &TableOptInfo,
    right_table: &TableOptInfo,
    right_scan: &PhysicalTableScan,
    on_expr: &Expr,
    join_type: JoinType,
) -> (JoinStrategy, Option<String>, f64) {
    let match_prob = CardinalityEstimator::estimate_semi_match_probability(
        left_rows,
        left_table.stats.as_ref(),
        right_scan.estimated_rows,
        right_table.stats.as_ref(),
        on_expr,
        &right_table.schema,
    );

    // 1. NestedLoop Strategy
    let nl_cost = if join_type == JoinType::Semi {
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
        let hash_cost = if join_type == JoinType::Semi {
            CostModel::hash_semi_cost(
                left_cost,
                left_rows,
                right_scan.cost,
                right_scan.estimated_rows,
                crate::query::DEFAULT_JOIN_MEMORY_BUDGET_BYTES,
            )
        } else {
            CostModel::hash_anti_cost(
                left_cost,
                left_rows,
                right_scan.cost,
                right_scan.estimated_rows,
                crate::query::DEFAULT_JOIN_MEMORY_BUDGET_BYTES,
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
                    left_rows,
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

    (best_strategy, best_index, best_cost)
}

/// Cost-based and statistics-aware query optimizer and physical access-path planner.
pub struct Optimizer;

impl Optimizer {
    /// Optimizes a bound SQL query (`BoundSelect`) into a deterministic cost-based `PhysicalPlan`.
    pub fn optimize(bound: &BoundSelect, tables: &[TableOptInfo]) -> Result<PhysicalPlan> {
        let is_multi_table = !bound.joins.is_empty();

        let where_conjuncts = match &bound.filter {
            Some(f) => split_conjunction(f),
            None => Vec::new(),
        };

        if !is_multi_table {
            let base_table_scan = plan_table_scan(&tables[0], where_conjuncts, false);
            let mut cur_cost = base_table_scan.cost;
            let cur_rows = base_table_scan.estimated_rows;

            if !bound.order_by.is_empty() {
                cur_cost += CostModel::sort_cost(cur_rows);
            }
            if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
                cur_cost += CostModel::aggregate_cost(cur_rows);
            }
            if !bound.window_exprs.is_empty() {
                cur_cost += CostModel::window_cost(cur_rows);
            }

            return Ok(PhysicalPlan {
                ctes: bound.ctes.clone(),
                base_table: base_table_scan,
                joins: Vec::new(),
                post_join_filter: None,
                group_by: bound.group_by.clone(),
                aggregates: bound.aggregates.clone(),
                having: bound.having.clone(),
                window_exprs: bound.window_exprs.clone(),
                projections: bound.projections.clone(),
                order_by: bound.order_by.clone(),
                offset: bound.offset,
                limit: bound.limit,
                set_op: None,
                logical_rewrites: Vec::new(),
                total_cost: cur_cost,
                estimated_rows: cur_rows,
            });
        }

        // Multi-table query: check if all joins are INNER JOINs eligible for full reordering
        let has_any_stats = tables.iter().any(|t| t.stats.is_some());
        let all_inner = bound.joins.iter().all(|j| j.join_type == JoinType::Inner);

        if all_inner && tables.len() >= 2 && has_any_stats {
            return optimize_all_inner_joins(bound, tables, &where_conjuncts);
        }

        // Query with OUTER JOIN barriers or unanalyzed default sequential ordering
        optimize_with_outer_barriers(bound, tables, &where_conjuncts)
    }
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
}

/// Analyzes local predicates for a single table and selects the optimal cost-based access path.
fn plan_table_scan(
    table: &TableOptInfo,
    conjuncts: Vec<Expr>,
    _is_multi_table: bool,
) -> PhysicalTableScan {
    let qualifier = table.qualifier();
    // 1. Unqualify conjuncts so they match raw table schema column names
    let unqual_conjuncts: Vec<Expr> = conjuncts
        .iter()
        .map(|c| {
            let unqual = unqualify_expr(c, qualifier);
            unqualify_expr(&unqual, &table.name)
        })
        .collect();

    // If this is a CTE / in-memory relation, it does not possess a physical B-Tree index or primary key path.
    if table.is_cte {
        let residual_filter = recombine_conjunction(unqual_conjuncts.clone());
        let estimated_rows = CardinalityEstimator::estimate_scan_cardinality(
            table.stats.as_ref(),
            residual_filter.as_ref(),
            &table.schema,
        );
        let cost = CostModel::table_scan_cost(
            CardinalityEstimator::estimate_table_rows(table.stats.as_ref()),
            unqual_conjuncts.len(),
        );

        return PhysicalTableScan {
            table_name: table.name.clone(),
            table_alias: table.alias.clone(),
            access_path: AccessPath::FullTableScan,
            residual_filter,
            estimated_rows,
            cost,
        };
    }

    let schema = &table.schema;
    let pk_col = schema.primary_key_column();
    let pk_name = &pk_col.name;
    let pk_type = pk_col.data_type;

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
        residual_filter: full_residual,
        estimated_rows: full_rows,
        cost: full_cost,
        priority: 3,
        index_id: None,
        index_name: None,
    });

    // Candidate 2: Primary Key Point Lookup
    for (idx, conj) in unqual_conjuncts.iter().enumerate() {
        if let Some(val) = extract_col_equality(conj, pk_name, pk_type) {
            if !val.is_null() {
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
        });
    }

    // Candidate 4: Secondary Indexes
    for sec_meta in &table.indexes {
        if sec_meta.column_indices.len() <= 1 {
            let sec_col = &schema.columns[sec_meta.column_idx];
            let col_name = &sec_col.name;
            let col_type = sec_col.data_type;

            // 4a. Point Lookups
            for (idx, conj) in unqual_conjuncts.iter().enumerate() {
                if let Some(val) = extract_col_equality(conj, col_name, col_type) {
                    if !val.is_null() {
                        let mut residual = unqual_conjuncts.clone();
                        residual.remove(idx);
                        let residual_filter = recombine_conjunction(residual);
                        let est_rows = CardinalityEstimator::estimate_sec_point_rows(
                            table.stats.as_ref(),
                            sec_meta,
                        );
                        let cost = CostModel::secondary_point_lookup_cost(est_rows);
                        candidates.push(AccessPathCandidate {
                            access_path: AccessPath::SecondaryIndexScan {
                                index_name: sec_meta.name.clone(),
                                index_id: sec_meta.index_id,
                                scan_type: SecondaryScanType::PointLookup(val),
                            },
                            residual_filter,
                            estimated_rows: est_rows,
                            cost,
                            priority: 2,
                            index_id: Some(sec_meta.index_id),
                            index_name: Some(sec_meta.name.clone()),
                        });
                    }
                }
            }

            // 4b. Range Scans
            let (sec_start, sec_end, used_indices) =
                extract_col_range_bounds(&unqual_conjuncts, col_name, col_type);
            if sec_start.is_some() || sec_end.is_some() {
                let residual: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !used_indices.contains(i))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual);
                let est_rows = CardinalityEstimator::estimate_sec_range_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    sec_start.as_ref(),
                    sec_end.as_ref(),
                );
                let cost = CostModel::secondary_range_scan_cost(est_rows);
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
                    cost,
                    priority: 2,
                    index_id: Some(sec_meta.index_id),
                    index_name: Some(sec_meta.name.clone()),
                });
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
                let residual_filter = recombine_conjunction(residual);
                let est_rows =
                    CardinalityEstimator::estimate_sec_point_rows(table.stats.as_ref(), sec_meta);
                let cost = CostModel::secondary_point_lookup_cost(est_rows);
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::SecondaryIndexScan {
                        index_name: sec_meta.name.clone(),
                        index_id: sec_meta.index_id,
                        scan_type: SecondaryScanType::CompositePointLookup(prefix_values),
                    },
                    residual_filter,
                    estimated_rows: est_rows,
                    cost,
                    priority: 2,
                    index_id: Some(sec_meta.index_id),
                    index_name: Some(sec_meta.name.clone()),
                });
            } else if m > 0 {
                // Proper composite prefix scan
                let residual: Vec<Expr> = unqual_conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !used_indices.contains(i))
                    .map(|(_, c)| c.clone())
                    .collect();
                let residual_filter = recombine_conjunction(residual);
                let est_rows = CardinalityEstimator::estimate_sec_prefix_rows(
                    table.stats.as_ref(),
                    sec_meta,
                    m,
                );
                let cost = CostModel::secondary_range_scan_cost(est_rows);
                candidates.push(AccessPathCandidate {
                    access_path: AccessPath::SecondaryIndexScan {
                        index_name: sec_meta.name.clone(),
                        index_id: sec_meta.index_id,
                        scan_type: SecondaryScanType::PrefixScan(prefix_values),
                    },
                    residual_filter,
                    estimated_rows: est_rows,
                    cost,
                    priority: 2,
                    index_id: Some(sec_meta.index_id),
                    index_name: Some(sec_meta.name.clone()),
                });
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

    let best = candidates.remove(0);

    PhysicalTableScan {
        table_name: table.name.clone(),
        table_alias: table.alias.clone(),
        access_path: best.access_path,
        residual_filter: best.residual_filter,
        estimated_rows: best.estimated_rows,
        cost: best.cost,
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
