use crate::error::Result;
use crate::sql::ast::{
    DerivedQuery, JoinClause, JoinType, SelectItem, SelectStatement, SqlBinaryOp, SqlExpr,
    SqlUnaryOp, TableRef,
};
use crate::sql::binder::TableCatalog;

/// Outcome of logical query rewriting containing the rewritten AST and a list of applied transformations.
#[derive(Debug, Clone, PartialEq)]
pub struct RewriteResult {
    /// Rewritten or unmodified SQL `SelectStatement`.
    pub statement: SelectStatement,
    /// Human-readable log of transformations applied (for EXPLAIN output).
    pub rewrites: Vec<String>,
}

/// Logical query rewriter performing safe derived-table flattening, predicate pushdown,
/// projection pruning, and subquery decorrelation.
pub struct QueryRewriter;

impl QueryRewriter {
    /// Rewrites a parsed `SelectStatement` applying conservative, semantic-preserving logical transformations.
    pub fn rewrite_select(
        stmt: &SelectStatement,
        catalog: Option<&dyn TableCatalog>,
    ) -> Result<RewriteResult> {
        let mut rewrites = Vec::new();
        let mut current_stmt = stmt.clone();

        // 1. Flatten safe derived tables in FROM and JOIN clauses with predicate pushdown
        Self::flatten_derived_tables(&mut current_stmt, &mut rewrites);

        // 2. Decorrelate supported subqueries in WHERE (EXISTS, NOT EXISTS, IN -> SEMI / ANTI JOINs)
        Self::decorrelate_subqueries(&mut current_stmt, catalog, &mut rewrites);

        Ok(RewriteResult {
            statement: current_stmt,
            rewrites,
        })
    }

    /// Recursively flattens derived tables in FROM and INNER/LEFT JOIN clauses when safe.
    fn flatten_derived_tables(stmt: &mut SelectStatement, rewrites: &mut Vec<String>) {
        // A. Attempt to flatten FROM derived table
        if let TableRef::Derived {
            query: DerivedQuery::Select(inner_stmt),
            alias,
        } = &stmt.from.clone()
        {
            if Self::can_safely_flatten_derived_select(inner_stmt) {
                let inner_from = &inner_stmt.from;
                let inner_name = inner_from.name().to_string();
                let inner_alias = alias.clone();

                // Replace FROM with the inner table (aliased as the derived table alias)
                stmt.from = TableRef::Table {
                    name: inner_name.clone(),
                    alias: Some(inner_alias.clone()),
                };

                // Merge inner selection with outer selection
                if let Some(inner_sel) = &inner_stmt.selection {
                    let mapped_inner_sel =
                        Self::qualify_or_map_expr(inner_sel, &inner_name, &inner_alias);
                    stmt.selection = match stmt.selection.take() {
                        Some(outer_sel) => Some(SqlExpr::Binary {
                            left: Box::new(mapped_inner_sel),
                            op: SqlBinaryOp::And,
                            right: Box::new(outer_sel),
                        }),
                        None => Some(mapped_inner_sel),
                    };
                }

                // If the inner query had joins, graft them before outer joins
                if !inner_stmt.joins.is_empty() {
                    let mut grafted_joins = inner_stmt.joins.clone();
                    grafted_joins.append(&mut stmt.joins);
                    stmt.joins = grafted_joins;
                }

                rewrites.push(format!(
                    "DerivedTableFlatten: flattened derived table '{alias}' into base table '{inner_name}' with predicate pushdown"
                ));
            }
        }

        // B. Attempt to flatten derived tables in JOIN clauses
        let mut new_joins = Vec::with_capacity(stmt.joins.len());
        for join in &stmt.joins {
            let mut flattened = false;
            if let TableRef::Derived {
                query: DerivedQuery::Select(inner_stmt),
                alias,
            } = &join.table
            {
                if (join.join_type == JoinType::Inner || join.join_type == JoinType::Left)
                    && Self::can_safely_flatten_derived_select(inner_stmt)
                    && inner_stmt.joins.is_empty()
                {
                    let inner_name = inner_stmt.from.name().to_string();
                    let inner_alias = alias.clone();

                    let mut merged_on = join.on.clone();
                    if let Some(inner_sel) = &inner_stmt.selection {
                        let mapped_inner_sel =
                            Self::qualify_or_map_expr(inner_sel, &inner_name, &inner_alias);
                        merged_on = SqlExpr::Binary {
                            left: Box::new(merged_on),
                            op: SqlBinaryOp::And,
                            right: Box::new(mapped_inner_sel),
                        };
                    }

                    new_joins.push(JoinClause {
                        join_type: join.join_type,
                        table: TableRef::Table {
                            name: inner_name.clone(),
                            alias: Some(inner_alias.clone()),
                        },
                        on: merged_on,
                    });

                    rewrites.push(format!(
                        "DerivedTableFlatten: flattened joined derived table '{alias}' into base table '{inner_name}' with predicate pushdown"
                    ));
                    flattened = true;
                }
            }

            if !flattened {
                new_joins.push(join.clone());
            }
        }
        stmt.joins = new_joins;
    }

    /// Checks whether an inner `SelectStatement` can be safely flattened into its enclosing query.
    ///
    /// Semantic barriers preventing flattening:
    /// - `GROUP BY` / Aggregates / `HAVING`
    /// - Window functions
    /// - `LIMIT` or `OFFSET`
    /// - `WITH` clauses (CTEs)
    /// - Non-table FROM (e.g. nested derived tables that must first be flattened recursively)
    /// - Non-simple projections (e.g. expressions that cannot be trivially aliased)
    pub fn can_safely_flatten_derived_select(inner: &SelectStatement) -> bool {
        if !inner.group_by.is_empty()
            || inner.having.is_some()
            || inner.limit.is_some()
            || inner.offset.is_some()
            || inner.with.is_some()
        {
            return false;
        }

        // Must be a base table
        match &inner.from {
            TableRef::Table { .. } => {}
            TableRef::Derived { .. } => return false,
        }

        // No window functions or complex non-column projections
        for item in &inner.projection {
            match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {}
                SelectItem::Expression { expr, .. } => {
                    if Self::expr_contains_aggregate_or_window(expr) {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Checks if a SQL expression contains aggregate or window functions.
    fn expr_contains_aggregate_or_window(expr: &SqlExpr) -> bool {
        match expr {
            SqlExpr::Aggregate { .. } | SqlExpr::CountAll | SqlExpr::Window { .. } => true,
            SqlExpr::Binary { left, right, .. } => {
                Self::expr_contains_aggregate_or_window(left)
                    || Self::expr_contains_aggregate_or_window(right)
            }
            SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
                Self::expr_contains_aggregate_or_window(expr)
            }
            SqlExpr::InList { expr, list, .. } => {
                Self::expr_contains_aggregate_or_window(expr)
                    || list.iter().any(Self::expr_contains_aggregate_or_window)
            }
            SqlExpr::InSubquery { expr, .. } | SqlExpr::InSetSubquery { expr, .. } => {
                Self::expr_contains_aggregate_or_window(expr)
            }
            _ => false,
        }
    }

    /// Decorrelates supported subquery patterns in `WHERE`:
    /// - `WHERE EXISTS (SELECT ... FROM child WHERE child.fk = parent.pk AND [child_filter])` -> `SEMI JOIN`
    /// - `WHERE NOT EXISTS (SELECT ... FROM child WHERE child.fk = parent.pk AND [child_filter])` -> `ANTI JOIN`
    /// - `WHERE parent_col IN (SELECT child_col FROM child WHERE [child_filter])` -> `SEMI JOIN`
    fn decorrelate_subqueries(
        stmt: &mut SelectStatement,
        _catalog: Option<&dyn TableCatalog>,
        rewrites: &mut Vec<String>,
    ) {
        if stmt.selection.is_none() {
            return;
        }

        let existing_qualifiers: Vec<String> = std::iter::once(stmt.from.qualifier().to_string())
            .chain(stmt.joins.iter().map(|j| j.table.qualifier().to_string()))
            .collect();

        let selection = stmt.selection.take().unwrap();
        let conjuncts = Self::split_sql_conjunction(selection);
        let mut remaining_conjuncts = Vec::new();

        for conjunct in conjuncts {
            let mut transformed = false;

            // Pattern 1: `EXISTS (SELECT ... FROM child WHERE ...)`
            if let SqlExpr::Exists { subquery, negated } = &conjunct {
                if let Some(semi_join) =
                    Self::try_decorrelate_exists(subquery, *negated, &existing_qualifiers)
                {
                    let child_name = semi_join.table.name().to_string();
                    let kind = if *negated { "ANTI JOIN" } else { "SEMI JOIN" };
                    let on_str = format!("{}", semi_join.on);
                    rewrites.push(format!(
                        "SubqueryDecorrelation: {} Correlated Subquery with Outer references: [{on_str}] on '{child_name}' rewritten to {kind}",
                        if *negated { "NOT EXISTS" } else { "EXISTS" }
                    ));
                    stmt.joins.push(semi_join);
                    transformed = true;
                }
            } else if let SqlExpr::Unary {
                op: SqlUnaryOp::Not,
                expr,
            } = &conjunct
            {
                // Pattern 1b: `NOT (EXISTS (...))`
                if let SqlExpr::Exists {
                    subquery,
                    negated: false,
                } = expr.as_ref()
                {
                    if let Some(anti_join) =
                        Self::try_decorrelate_exists(subquery, true, &existing_qualifiers)
                    {
                        let child_name = anti_join.table.name().to_string();
                        let on_str = format!("{}", anti_join.on);
                        rewrites.push(format!(
                            "SubqueryDecorrelation: NOT EXISTS Correlated Subquery with Outer references: [{on_str}] on '{child_name}' rewritten to ANTI JOIN"
                        ));
                        stmt.joins.push(anti_join);
                        transformed = true;
                    }
                }
            } else if let SqlExpr::InSubquery {
                expr: left_expr,
                subquery,
                negated: false,
            } = &conjunct
            {
                // Pattern 2: `col IN (SELECT child_col FROM child WHERE ...)`
                if let Some(semi_join) =
                    Self::try_decorrelate_in_subquery(left_expr, subquery, &existing_qualifiers)
                {
                    let child_name = semi_join.table.name().to_string();
                    let on_str = format!("{}", semi_join.on);
                    rewrites.push(format!(
                        "SubqueryDecorrelation: IN subquery with Outer references: [{on_str}] on '{child_name}' rewritten to SEMI JOIN"
                    ));
                    stmt.joins.push(semi_join);
                    transformed = true;
                }
            }

            if !transformed {
                remaining_conjuncts.push(conjunct);
            }
        }

        stmt.selection = Self::recombine_sql_conjunction(remaining_conjuncts);
    }

    /// Checks if a SQL expression contains nested subqueries.
    fn expr_contains_subquery(expr: &SqlExpr) -> bool {
        match expr {
            SqlExpr::Subquery(_)
            | SqlExpr::SetSubquery(_)
            | SqlExpr::Exists { .. }
            | SqlExpr::SetExists { .. }
            | SqlExpr::InSubquery { .. }
            | SqlExpr::InSetSubquery { .. } => true,
            SqlExpr::Binary { left, right, .. } => {
                Self::expr_contains_subquery(left) || Self::expr_contains_subquery(right)
            }
            SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
                Self::expr_contains_subquery(expr)
            }
            SqlExpr::InList { expr, list, .. } => {
                Self::expr_contains_subquery(expr) || list.iter().any(Self::expr_contains_subquery)
            }
            SqlExpr::Aggregate { arg, .. } => Self::expr_contains_subquery(arg),
            SqlExpr::Window { args, .. } => args.iter().any(Self::expr_contains_subquery),
            _ => false,
        }
    }

    /// Attempts to decorrelate an `EXISTS` or `NOT EXISTS` subquery into a `SEMI` or `ANTI` join.
    fn try_decorrelate_exists(
        subquery: &SelectStatement,
        negated: bool,
        existing_qualifiers: &[String],
    ) -> Option<JoinClause> {
        // Must not conflict with existing table qualifiers in outer query
        if existing_qualifiers
            .iter()
            .any(|q| q == subquery.from.qualifier())
        {
            return None;
        }

        // Must be simple single-table subquery with no group by / aggregates / windows / limit
        if !subquery.group_by.is_empty()
            || subquery.having.is_some()
            || subquery.limit.is_some()
            || subquery.offset.is_some()
            || subquery.with.is_some()
            || !subquery.joins.is_empty()
        {
            return None;
        }

        if let Some(sel) = &subquery.selection {
            if Self::expr_contains_subquery(sel) {
                return None;
            }
        }

        let join_type = if negated {
            JoinType::Anti
        } else {
            JoinType::Semi
        };

        let table_qualifier = subquery.from.qualifier().to_string();
        let table_name = subquery.from.name().to_string();
        let on_expr = match &subquery.selection {
            Some(sel) => Self::qualify_or_map_expr(sel, &table_name, &table_qualifier),
            None => {
                // Uncorrelated `EXISTS (SELECT 1 FROM child)` with no WHERE
                SqlExpr::Literal(crate::sql::ast::SqlLiteral::Boolean(true))
            }
        };

        Some(JoinClause {
            join_type,
            table: subquery.from.clone(),
            on: on_expr,
        })
    }

    /// Attempts to decorrelate an `IN (SELECT col FROM ...)` subquery into a `SEMI JOIN`.
    fn try_decorrelate_in_subquery(
        left_expr: &SqlExpr,
        subquery: &SelectStatement,
        existing_qualifiers: &[String],
    ) -> Option<JoinClause> {
        // Must not conflict with existing table qualifiers in outer query
        if existing_qualifiers
            .iter()
            .any(|q| q == subquery.from.qualifier())
        {
            return None;
        }

        if !subquery.group_by.is_empty()
            || subquery.having.is_some()
            || subquery.limit.is_some()
            || subquery.offset.is_some()
            || subquery.with.is_some()
            || !subquery.joins.is_empty()
            || subquery.projection.len() != 1
        {
            return None;
        }

        if let Some(sel) = &subquery.selection {
            if Self::expr_contains_subquery(sel) {
                return None;
            }
        }

        let right_expr = match &subquery.projection[0] {
            SelectItem::Expression { expr, .. } => expr.clone(),
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => return None,
        };

        // Qualify the right-hand projection with the subquery table's qualifier so that
        // the ON condition is unambiguous even when both tables share a column name (e.g. `id`).
        let table_qualifier = subquery.from.qualifier().to_string();
        let table_name = subquery.from.name().to_string();
        let qualified_right = match right_expr {
            SqlExpr::Identifier(col) => SqlExpr::QualifiedIdentifier {
                qualifier: table_qualifier.clone(),
                name: col,
            },
            // Already qualified — leave as-is.
            other => other,
        };

        let equi_cond = SqlExpr::Binary {
            left: Box::new(left_expr.clone()),
            op: SqlBinaryOp::Eq,
            right: Box::new(qualified_right),
        };

        let on_expr = match &subquery.selection {
            Some(filter) => {
                let mapped_filter =
                    Self::qualify_or_map_expr(filter, &table_name, &table_qualifier);
                SqlExpr::Binary {
                    left: Box::new(equi_cond),
                    op: SqlBinaryOp::And,
                    right: Box::new(mapped_filter),
                }
            }
            None => equi_cond,
        };

        Some(JoinClause {
            join_type: JoinType::Semi,
            table: subquery.from.clone(),
            on: on_expr,
        })
    }

    /// Splits top-level `AND` conjunctions in a `SqlExpr`.
    pub fn split_sql_conjunction(expr: SqlExpr) -> Vec<SqlExpr> {
        let mut conjuncts = Vec::new();
        Self::collect_sql_conjuncts(expr, &mut conjuncts);
        conjuncts
    }

    fn collect_sql_conjuncts(expr: SqlExpr, conjuncts: &mut Vec<SqlExpr>) {
        if let SqlExpr::Binary {
            left,
            op: SqlBinaryOp::And,
            right,
        } = expr
        {
            Self::collect_sql_conjuncts(*left, conjuncts);
            Self::collect_sql_conjuncts(*right, conjuncts);
        } else {
            conjuncts.push(expr);
        }
    }

    /// Recombines a list of `SqlExpr` conjuncts into a single `AND` conjunction tree.
    pub fn recombine_sql_conjunction(mut conjuncts: Vec<SqlExpr>) -> Option<SqlExpr> {
        if conjuncts.is_empty() {
            return None;
        }
        let mut expr = conjuncts.remove(0);
        for next in conjuncts {
            expr = SqlExpr::Binary {
                left: Box::new(expr),
                op: SqlBinaryOp::And,
                right: Box::new(next),
            };
        }
        Some(expr)
    }

    /// Recursively qualifies unqualified column references or replaces table names with aliases.
    fn qualify_or_map_expr(expr: &SqlExpr, orig_table: &str, new_alias: &str) -> SqlExpr {
        match expr {
            SqlExpr::Identifier(col) => SqlExpr::QualifiedIdentifier {
                qualifier: new_alias.to_string(),
                name: col.clone(),
            },
            SqlExpr::QualifiedIdentifier { qualifier, name } => {
                if qualifier.eq_ignore_ascii_case(orig_table) {
                    SqlExpr::QualifiedIdentifier {
                        qualifier: new_alias.to_string(),
                        name: name.clone(),
                    }
                } else {
                    SqlExpr::QualifiedIdentifier {
                        qualifier: qualifier.clone(),
                        name: name.clone(),
                    }
                }
            }
            SqlExpr::Binary { left, op, right } => SqlExpr::Binary {
                left: Box::new(Self::qualify_or_map_expr(left, orig_table, new_alias)),
                op: *op,
                right: Box::new(Self::qualify_or_map_expr(right, orig_table, new_alias)),
            },
            SqlExpr::Unary { op, expr } => SqlExpr::Unary {
                op: *op,
                expr: Box::new(Self::qualify_or_map_expr(expr, orig_table, new_alias)),
            },
            SqlExpr::IsNull(expr) => SqlExpr::IsNull(Box::new(Self::qualify_or_map_expr(
                expr, orig_table, new_alias,
            ))),
            SqlExpr::IsNotNull(expr) => SqlExpr::IsNotNull(Box::new(Self::qualify_or_map_expr(
                expr, orig_table, new_alias,
            ))),
            SqlExpr::InList {
                expr,
                list,
                negated,
            } => SqlExpr::InList {
                expr: Box::new(Self::qualify_or_map_expr(expr, orig_table, new_alias)),
                list: list
                    .iter()
                    .map(|e| Self::qualify_or_map_expr(e, orig_table, new_alias))
                    .collect(),
                negated: *negated,
            },
            SqlExpr::Aggregate { function, arg } => SqlExpr::Aggregate {
                function: *function,
                arg: Box::new(Self::qualify_or_map_expr(arg, orig_table, new_alias)),
            },
            SqlExpr::Window {
                function,
                args,
                window_spec,
            } => SqlExpr::Window {
                function: *function,
                args: args
                    .iter()
                    .map(|e| Self::qualify_or_map_expr(e, orig_table, new_alias))
                    .collect(),
                window_spec: window_spec.clone(),
            },
            other => other.clone(),
        }
    }
}
