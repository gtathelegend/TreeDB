use std::collections::HashSet;
use std::fmt;

use crate::engine::catalog::{IndexId, IndexMetadata};
use crate::engine::types::{DataType, Schema, Value};
use crate::error::Result;
use crate::query::expr::{AggregateExpr, BinaryOp, Expr};
use crate::query::operator::SortKey;
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
    /// Exact point lookup on an indexed column value.
    PointLookup(Value),
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
}

impl PhysicalTableScan {
    /// Returns the active qualifier (alias if present, else table name).
    pub fn qualifier(&self) -> &str {
        self.table_alias.as_deref().unwrap_or(&self.table_name)
    }
}

/// Physical relational join node.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalJoin {
    /// Relational join type (Inner or Left).
    pub join_type: JoinType,
    /// Right-hand table scan node.
    pub right: PhysicalTableScan,
    /// Join condition boolean predicate.
    pub on: Expr,
}

/// Complete physical execution plan produced by the rule-based optimizer.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPlan {
    /// Statement-local CTE definitions.
    pub ctes: Vec<crate::sql::binder::BoundCte>,
    /// Base relation scan node (FROM table).
    pub base_table: PhysicalTableScan,
    /// Physical joins in left-to-right evaluation order.
    pub joins: Vec<PhysicalJoin>,
    /// Global post-join filter predicate (WHERE condition across multiple tables).
    pub post_join_filter: Option<Expr>,
    /// Grouping expressions (GROUP BY).
    pub group_by: Vec<Expr>,
    /// Aggregate expressions.
    pub aggregates: Vec<AggregateExpr>,
    /// Post-aggregation filter predicate (HAVING).
    pub having: Option<Expr>,
    /// Output projection expressions with optional aliases.
    pub projections: Vec<(Expr, Option<String>)>,
    /// Sort keys (ORDER BY).
    pub order_by: Vec<SortKey>,
    /// Offset pagination.
    pub offset: Option<usize>,
    /// Limit pagination.
    pub limit: Option<usize>,
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

        if !self.ctes.is_empty() {
            for cte in &self.ctes {
                write_indent(f, indent)?;
                writeln!(f, "CTE {}", cte.name)?;
                if !cte.dependencies.is_empty() {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "depends on: [{}]", cte.dependencies.join(", "))?;
                }
                let plan_str = format!("{}", cte.plan);
                for line in plan_str.lines() {
                    write_indent(f, indent + 1)?;
                    writeln!(f, "{line}")?;
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
            if let Some(rf) = &self.base_table.residual_filter {
                format_subqueries_in_expr(rf, f, indent + 1)?;
            }
        } else {
            for (idx, join) in self.joins.iter().enumerate() {
                write_indent(f, indent)?;
                writeln!(
                    f,
                    "NestedLoopJoin ({:?}, on: [{}])",
                    join.join_type, join.on
                )?;
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
        Expr::Literal(_) | Expr::Column(_) | Expr::OuterColumn { .. } => Ok(()),
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
        }
    }

    /// Returns the active qualifier (alias if present, otherwise table name).
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// Rule-based query optimizer and physical access-path planner.
pub struct Optimizer;

impl Optimizer {
    /// Optimizes a bound SQL query (`BoundSelect`) into a deterministic `PhysicalPlan`.
    ///
    /// # Optimization Pipeline
    /// 1. **Predicate Decomposition**: Decomposes `WHERE` and `ON` clauses into top-level `AND` conjuncts.
    /// 2. **Relation Affiliation & Pushdown Classification**:
    ///    - Single-table predicates are identified.
    ///    - Pushed down to table scans where safe (preserving LEFT JOIN semantics).
    ///    - Multi-table predicates and unsafe outer-join filters remain in `post_join_filter` or `ON`.
    /// 3. **Access Path Selection**:
    ///    - For each table, candidate local predicates are analyzed with deterministic priority:
    ///      1. `PrimaryPointLookup`
    ///      2. `PrimaryRangeScan`
    ///      3. `SecondaryIndexScan` (sorted by `IndexId` ascending)
    ///      4. `FullTableScan`
    ///    - Unconsumed candidate predicates are preserved in `residual_filter`.
    /// 4. **Physical Plan Assembly**: Constructs the `PhysicalPlan` preserving left-to-right join order.
    pub fn optimize(bound: &BoundSelect, tables: &[TableOptInfo]) -> Result<PhysicalPlan> {
        let is_multi_table = !bound.joins.is_empty();

        // 1. Deconstruct WHERE clause into conjuncts
        let where_conjuncts = match &bound.filter {
            Some(f) => split_conjunction(f),
            None => Vec::new(),
        };

        // 2. Classify WHERE conjuncts per table
        let mut table_local_conjuncts: Vec<Vec<Expr>> = vec![Vec::new(); tables.len()];
        let mut post_join_conjuncts: Vec<Expr> = Vec::new();

        for conjunct in where_conjuncts {
            let refs = expr_referenced_qualifiers(&conjunct);
            if !is_multi_table {
                // Single table query: all conjuncts belong to base table
                table_local_conjuncts[0].push(conjunct);
            } else if refs.len() == 1 {
                let target_qualifier = refs.iter().next().unwrap();
                // Find table index
                if let Some(tbl_idx) = tables
                    .iter()
                    .position(|t| t.qualifier() == target_qualifier)
                {
                    if tbl_idx == 0 {
                        // Base table: always safe to push down WHERE filter
                        table_local_conjuncts[0].push(conjunct);
                    } else {
                        // Joined table: check if any join up to tbl_idx is a LEFT JOIN
                        // If tbl_idx is on the right side of a LEFT JOIN, pushing down WHERE
                        // is UNSAFE because WHERE must eliminate NULL-padded unmatched rows.
                        let is_right_of_left_join =
                            bound.joins[tbl_idx - 1].join_type == JoinType::Left;
                        if is_right_of_left_join {
                            post_join_conjuncts.push(conjunct);
                        } else {
                            table_local_conjuncts[tbl_idx].push(conjunct);
                        }
                    }
                } else {
                    post_join_conjuncts.push(conjunct);
                }
            } else {
                // Multi-table predicate (or literal constant like 1=1)
                post_join_conjuncts.push(conjunct);
            }
        }

        // 3. Process JOIN ON clauses
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
                        // Right-local predicate in INNER JOIN ON: push to right table
                        table_local_conjuncts[right_table_idx].push(on_conj);
                    } else if refs.len() == 1 && refs.contains(tables[0].qualifier()) {
                        // Left-local predicate in INNER JOIN ON: push to base table
                        table_local_conjuncts[0].push(on_conj);
                    } else {
                        remaining_on.push(on_conj);
                    }
                } else if bound_join.join_type == JoinType::Left {
                    if refs.len() == 1 && refs.contains(right_qualifier) {
                        // Right-local predicate in LEFT JOIN ON: safe to push to right table scan
                        // (matches only satisfying right rows; unmatched left rows still get NULL padded)
                        table_local_conjuncts[right_table_idx].push(on_conj);
                    } else {
                        // Multi-table or left-local ON predicate in LEFT JOIN remains in ON
                        remaining_on.push(on_conj);
                    }
                }
            }

            let on_expr =
                recombine_conjunction(remaining_on).unwrap_or(Expr::Literal(Value::Boolean(true)));
            optimized_joins.push((bound_join.join_type, on_expr));
        }

        // 4. Select Access Paths for Base Table and Joined Tables
        let base_table_scan =
            plan_table_scan(&tables[0], table_local_conjuncts[0].clone(), is_multi_table);

        let mut physical_joins = Vec::new();
        for (join_idx, (join_type, on_expr)) in optimized_joins.into_iter().enumerate() {
            let right_table_idx = join_idx + 1;
            let right_table_scan = plan_table_scan(
                &tables[right_table_idx],
                table_local_conjuncts[right_table_idx].clone(),
                is_multi_table,
            );
            physical_joins.push(PhysicalJoin {
                join_type,
                right: right_table_scan,
                on: on_expr,
            });
        }

        let post_join_filter = recombine_conjunction(post_join_conjuncts);

        Ok(PhysicalPlan {
            ctes: bound.ctes.clone(),
            base_table: base_table_scan,
            joins: physical_joins,
            post_join_filter,
            group_by: bound.group_by.clone(),
            aggregates: bound.aggregates.clone(),
            having: bound.having.clone(),
            projections: bound.projections.clone(),
            order_by: bound.order_by.clone(),
            offset: bound.offset,
            limit: bound.limit,
        })
    }
}

/// Analyzes local predicates for a single table and selects the optimal access path.
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

    let schema = &table.schema;
    let pk_col = schema.primary_key_column();
    let pk_name = &pk_col.name;
    let pk_type = pk_col.data_type;

    // Check 1: Primary Key Point Lookup
    for (idx, conj) in unqual_conjuncts.iter().enumerate() {
        if let Some(val) = extract_col_equality(conj, pk_name, pk_type) {
            if !val.is_null() {
                let mut residual = unqual_conjuncts.clone();
                residual.remove(idx);
                let residual_filter = recombine_conjunction(residual);
                return PhysicalTableScan {
                    table_name: table.name.clone(),
                    table_alias: table.alias.clone(),
                    access_path: AccessPath::PrimaryPointLookup { key: val },
                    residual_filter,
                };
            }
        }
    }

    // Check 2: Primary Key Range Scan
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
        return PhysicalTableScan {
            table_name: table.name.clone(),
            table_alias: table.alias.clone(),
            access_path: AccessPath::PrimaryRangeScan {
                start: pk_start,
                end: pk_end,
            },
            residual_filter,
        };
    }

    // Check 3: Secondary Index Scans (sorted deterministically by IndexId ascending)
    let mut sorted_indexes = table.indexes.clone();
    sorted_indexes.sort_by(|a, b| {
        a.index_id
            .cmp(&b.index_id)
            .then_with(|| a.name.cmp(&b.name))
    });

    for sec_meta in &sorted_indexes {
        let sec_col = &schema.columns[sec_meta.column_idx];
        let col_name = &sec_col.name;
        let col_type = sec_col.data_type;

        // 3a. Secondary Point Lookup
        for (idx, conj) in unqual_conjuncts.iter().enumerate() {
            if let Some(val) = extract_col_equality(conj, col_name, col_type) {
                if !val.is_null() {
                    let mut residual = unqual_conjuncts.clone();
                    residual.remove(idx);
                    let residual_filter = recombine_conjunction(residual);
                    return PhysicalTableScan {
                        table_name: table.name.clone(),
                        table_alias: table.alias.clone(),
                        access_path: AccessPath::SecondaryIndexScan {
                            index_name: sec_meta.name.clone(),
                            index_id: sec_meta.index_id,
                            scan_type: SecondaryScanType::PointLookup(val),
                        },
                        residual_filter,
                    };
                }
            }
        }

        // 3b. Secondary Range Scan
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
            return PhysicalTableScan {
                table_name: table.name.clone(),
                table_alias: table.alias.clone(),
                access_path: AccessPath::SecondaryIndexScan {
                    index_name: sec_meta.name.clone(),
                    index_id: sec_meta.index_id,
                    scan_type: SecondaryScanType::RangeScan {
                        start: sec_start,
                        end: sec_end,
                    },
                },
                residual_filter,
            };
        }
    }

    // Check 4: Full Table Scan fallback
    let residual_filter = recombine_conjunction(unqual_conjuncts);
    PhysicalTableScan {
        table_name: table.name.clone(),
        table_alias: table.alias.clone(),
        access_path: AccessPath::FullTableScan,
        residual_filter,
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
        Expr::Subquery(_) | Expr::Literal(_) => {}
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
            (Expr::Column(name), Expr::Literal(val)) if name == target_col => {
                if val.data_type() == target_type || val.is_null() {
                    Some(val.clone())
                } else {
                    None
                }
            }
            (Expr::Literal(val), Expr::Column(name)) if name == target_col => {
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
                    if name == target_col && val.data_type() == target_type && !val.is_null() =>
                {
                    if start.is_none() {
                        start = Some(val.clone());
                        used.insert(idx);
                    }
                }
                (Expr::Literal(val), Expr::Column(name))
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
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && end.is_none() =>
                {
                    end = Some(val.clone());
                    used.insert(idx);
                }
                (Expr::Literal(val), Expr::Column(name))
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
                    if name == target_col
                        && val.data_type() == target_type
                        && !val.is_null()
                        && start.is_none() =>
                {
                    // val <= col -> col >= val
                    start = Some(val.clone());
                    used.insert(idx);
                }
                (Expr::Column(name), Expr::Literal(_)) if name == target_col => {
                    // col <= val: B+ Tree scan is exclusive on end, so keep in residual filter
                }
                _ => {}
            },
            _ => {}
        }
    }

    (start, end, used)
}
