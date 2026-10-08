use std::cmp::Ordering;
use std::collections::HashMap;

use crate::engine::transaction::Transaction;
use crate::engine::types::{Column, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::Expr;
use crate::query::operator::{
    compare_values_for_sort, AggregateOperator, CanonicalValue, OffsetLimitOperator, QueryResult,
    SetOperationOperator, SortKey, WindowOperator,
};
use crate::sql::ast::{JoinType, SelectStatement, SetQuery, SetQueryBody, Statement};
use crate::sql::binder::BoundSelect;

/// Canonical representation of a table `Row` used for deterministic differential testing.
///
/// Ensures deterministic multiset comparison and handles all primitive types (including NaN floats,
/// negative zero, NULLs, strings, bytes, booleans, and 64-bit integers).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalRow(pub Vec<CanonicalValue>);

impl CanonicalRow {
    /// Constructs a `CanonicalRow` from a table `Row`.
    pub fn from_row(row: &Row) -> Self {
        Self(row.values.iter().map(CanonicalValue::from_value).collect())
    }

    /// Converts back to a table `Row`.
    pub fn to_row(&self) -> Row {
        Row::new(self.0.iter().map(|cv| cv.to_value()).collect())
    }
}

/// Asserts that two query execution results are differentially equivalent according to SQL relational semantics.
///
/// Verifies:
/// 1. Schema: column count, column names, and column data types.
/// 2. Row multiplicity: total row count.
/// 3. Rows:
///    - If `require_order` is `true`: exact sequence equality.
///    - If `require_order` is `false`: multiset equality (rows canonicalized, sorted, and compared).
pub fn assert_differential_equivalence(
    actual: &QueryResult,
    expected: &QueryResult,
    require_order: bool,
) {
    assert_eq!(
        actual.schema.columns.len(),
        expected.schema.columns.len(),
        "Column count mismatch:\n  actual:   {} cols\n  expected: {} cols",
        actual.schema.columns.len(),
        expected.schema.columns.len(),
    );

    for (i, (act_col, exp_col)) in actual
        .schema
        .columns
        .iter()
        .zip(expected.schema.columns.iter())
        .enumerate()
    {
        assert_eq!(
            act_col.data_type, exp_col.data_type,
            "Column {} ('{}') data type mismatch:\n  actual:   {:?}\n  expected: {:?}",
            i, act_col.name, act_col.data_type, exp_col.data_type,
        );
        assert_eq!(
            act_col.name, exp_col.name,
            "Column {} name mismatch:\n  actual:   '{}'\n  expected: '{}'",
            i, act_col.name, exp_col.name,
        );
    }

    assert_eq!(
        actual.rows.len(),
        expected.rows.len(),
        "Row count mismatch:\n  actual:   {} rows\n  expected: {} rows",
        actual.rows.len(),
        expected.rows.len(),
    );

    if require_order {
        for (i, (act_row, exp_row)) in actual.rows.iter().zip(expected.rows.iter()).enumerate() {
            let act_canon = CanonicalRow::from_row(act_row);
            let exp_canon = CanonicalRow::from_row(exp_row);
            assert_eq!(
                act_canon, exp_canon,
                "Row {} mismatch with required ordering:\n  actual:   {:?}\n  expected: {:?}",
                i, act_row, exp_row,
            );
        }
    } else {
        let mut act_sorted: Vec<CanonicalRow> =
            actual.rows.iter().map(CanonicalRow::from_row).collect();
        let mut exp_sorted: Vec<CanonicalRow> =
            expected.rows.iter().map(CanonicalRow::from_row).collect();
        act_sorted.sort();
        exp_sorted.sort();

        for (i, (act_canon, exp_canon)) in act_sorted.iter().zip(exp_sorted.iter()).enumerate() {
            assert_eq!(
                act_canon, exp_canon,
                "Row {} mismatch in multiset comparison:\n  actual:   {:?}\n  expected: {:?}",
                i, act_canon, exp_canon,
            );
        }
    }
}

/// Structured failure report emitted when a randomized differential test discovers a discrepancy.
#[derive(Debug, Clone)]
pub struct DifferentialFailureReport {
    pub seed: u64,
    pub step: usize,
    pub sql: String,
    pub error_message: String,
    pub chosen_strategy: Option<String>,
}

impl std::fmt::Display for DifferentialFailureReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "================ DIFFERENTIAL FAILURE REPORT ================"
        )?;
        writeln!(f, "Reproducible Seed: {}", self.seed)?;
        writeln!(f, "Operation Step:    {}", self.step)?;
        writeln!(f, "Executed SQL:      {}", self.sql)?;
        if let Some(strat) = &self.chosen_strategy {
            writeln!(f, "Physical Strategy: {}", strat)?;
        }
        writeln!(f, "Failure Reason:    {}", self.error_message)?;
        writeln!(
            f,
            "============================================================="
        )?;
        Ok(())
    }
}

/// Simple, unoptimized relational reference evaluator.
///
/// Prioritizes semantic correctness over execution performance.
/// Avoids cost-based access path selection, optimizer memo exploration, join reordering,
/// adaptive replanning, runtime feedback, sort elimination, and disk-spilling operators.
pub struct ReferenceEvaluator<'a, 'b> {
    pub txn: &'a mut Transaction<'b>,
}

impl<'a, 'b> ReferenceEvaluator<'a, 'b> {
    /// Creates a new `ReferenceEvaluator` operating on the given transaction.
    pub fn new(txn: &'a mut Transaction<'b>) -> Self {
        Self { txn }
    }

    /// Parses, binds, and executes a SQL query using the reference evaluation model.
    pub fn execute_sql(txn: &mut Transaction<'_>, sql: &str) -> Result<QueryResult> {
        let stmt = crate::sql::parse_statement(sql)?;
        let mut eval = ReferenceEvaluator::new(txn);
        eval.execute_statement(&stmt)
    }

    /// Executes a parsed AST statement using reference relational semantics.
    pub fn execute_statement(&mut self, stmt: &Statement) -> Result<QueryResult> {
        let mut cte_store = HashMap::new();
        match stmt {
            Statement::Select(select_ast) => self.execute_select(select_ast, &[], &mut cte_store),
            Statement::SetQuery(set_ast) => self.execute_set_query(set_ast, &[], &mut cte_store),
            other => Err(Error::UnsupportedStatement(format!("{:?}", other))),
        }
    }

    /// Executes a `SelectStatement` using reference relational semantics.
    pub fn execute_select(
        &mut self,
        select_ast: &SelectStatement,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<QueryResult> {
        // Evaluate in-scope CTE definitions
        if let Some(with_clause) = &select_ast.with {
            for cte in &with_clause.ctes {
                if !cte_store.contains_key(&cte.name) {
                    match &cte.query {
                        crate::sql::ast::CteQuery::NonRecursive(inner_sel) => {
                            let res = self.execute_select(inner_sel, outer_contexts, cte_store)?;
                            let schema = if let Some(cols) = &cte.columns {
                                if cols.len() != res.schema.columns.len() {
                                    return Err(Error::CteColumnCountMismatch {
                                        cte: cte.name.clone(),
                                        expected: cols.len(),
                                        found: res.schema.columns.len(),
                                    });
                                }
                                let renamed: Vec<Column> = cols
                                    .iter()
                                    .zip(res.schema.columns.iter())
                                    .map(|(name, orig)| {
                                        Column::new(name, orig.data_type, orig.nullable)
                                    })
                                    .collect();
                                Schema::new_projected(renamed)
                            } else {
                                res.schema
                            };
                            cte_store.insert(cte.name.clone(), (schema, res.rows));
                        }
                        crate::sql::ast::CteQuery::SetOperation(inner_set) => {
                            let res =
                                self.execute_set_query(inner_set, outer_contexts, cte_store)?;
                            let schema = if let Some(cols) = &cte.columns {
                                if cols.len() != res.schema.columns.len() {
                                    return Err(Error::CteColumnCountMismatch {
                                        cte: cte.name.clone(),
                                        expected: cols.len(),
                                        found: res.schema.columns.len(),
                                    });
                                }
                                let renamed: Vec<Column> = cols
                                    .iter()
                                    .zip(res.schema.columns.iter())
                                    .map(|(name, orig)| {
                                        Column::new(name, orig.data_type, orig.nullable)
                                    })
                                    .collect();
                                Schema::new_projected(renamed)
                            } else {
                                res.schema
                            };
                            cte_store.insert(cte.name.clone(), (schema, res.rows));
                        }
                        crate::sql::ast::CteQuery::Recursive { anchor, recursive } => {
                            let anchor_res =
                                self.execute_select(anchor, outer_contexts, cte_store)?;
                            let final_schema = if let Some(cols) = &cte.columns {
                                if cols.len() != anchor_res.schema.columns.len() {
                                    return Err(Error::CteColumnCountMismatch {
                                        cte: cte.name.clone(),
                                        expected: cols.len(),
                                        found: anchor_res.schema.columns.len(),
                                    });
                                }
                                let renamed: Vec<Column> = cols
                                    .iter()
                                    .zip(anchor_res.schema.columns.iter())
                                    .map(|(name, orig)| {
                                        Column::new(name, orig.data_type, orig.nullable)
                                    })
                                    .collect();
                                Schema::new_projected(renamed)
                            } else {
                                anchor_res.schema
                            };

                            let mut accumulated_rows = anchor_res.rows;
                            let mut working_rows = accumulated_rows.clone();
                            let mut iters = 0;

                            while !working_rows.is_empty() {
                                iters += 1;
                                if iters > 1000 {
                                    return Err(Error::RecursiveCteIterationLimit {
                                        cte: cte.name.clone(),
                                        iterations: iters,
                                        limit: 1000,
                                    });
                                }
                                cte_store
                                    .insert(cte.name.clone(), (final_schema.clone(), working_rows));
                                let next_res =
                                    self.execute_select(recursive, outer_contexts, cte_store)?;
                                let next_working = next_res.rows;
                                if next_working.is_empty() {
                                    break;
                                }
                                if accumulated_rows.len() + next_working.len() > 1_000_000 {
                                    return Err(Error::RecursiveCteRowLimit {
                                        cte: cte.name.clone(),
                                        rows: accumulated_rows.len() + next_working.len(),
                                        limit: 1_000_000,
                                    });
                                }
                                accumulated_rows.extend(next_working.clone());
                                working_rows = next_working;
                            }
                            cte_store.insert(cte.name.clone(), (final_schema, accumulated_rows));
                        }
                    }
                }
            }
        }

        // Bind the SELECT statement with any in-scope CTE definitions from cte_store
        let mut visible_bound_ctes = Vec::new();
        for (name, (schema, _)) in cte_store.iter() {
            visible_bound_ctes.push(crate::sql::binder::BoundCte {
                name: name.clone(),
                output_schema: schema.clone(),
                kind: crate::sql::binder::BoundCteKind::NonRecursive {
                    plan: crate::query::optimizer::PhysicalPlan::dummy(name),
                    decision: crate::query::materialize::MaterializationDecision::Inline,
                    materialize_id: None,
                    consumer_count: 1,
                    estimated_inline_cost: 0.0,
                    estimated_materialize_cost: 0.0,
                    fingerprint: None,
                },
                dependencies: Vec::new(),
            });
        }
        let mut ast_for_binding = select_ast.clone();
        ast_for_binding.with = None;
        let bound = self
            .txn
            .bind_select_with_outer_ctes(&ast_for_binding, &visible_bound_ctes)?;
        self.execute_bound_select(&bound, outer_contexts, cte_store)
    }

    /// Executes a `SetQuery` using reference relational semantics.
    pub fn execute_set_query(
        &mut self,
        set_ast: &SetQuery,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<QueryResult> {
        let (schema, mut rows) =
            self.execute_set_query_body(&set_ast.body, outer_contexts, cte_store)?;

        // Apply ORDER BY if present
        if !set_ast.order_by.is_empty() {
            let sort_keys =
                self.txn
                    .bind_order_by_for_schema(&schema, &set_ast.order_by, &[], None)?;
            rows = self.sort_rows(rows, &schema, &sort_keys, outer_contexts, cte_store)?;
        }

        // Apply OFFSET / LIMIT if present
        let final_rows = OffsetLimitOperator::apply(
            rows,
            set_ast.offset.map(|o| o as usize),
            set_ast.limit.map(|l| l as usize),
        );
        Ok(QueryResult::new(schema, final_rows))
    }

    fn execute_set_query_body(
        &mut self,
        body: &SetQueryBody,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(Schema, Vec<Row>)> {
        match body {
            SetQueryBody::Select(sel) => {
                let res = self.execute_select(sel, outer_contexts, cte_store)?;
                Ok((res.schema, res.rows))
            }
            SetQueryBody::Parenthesized(inner) => {
                let res = self.execute_set_query(inner, outer_contexts, cte_store)?;
                Ok((res.schema, res.rows))
            }
            SetQueryBody::SetOperation { op, left, right } => {
                let (left_schema, left_rows) =
                    self.execute_set_query_body(left, outer_contexts, cte_store)?;
                let (right_schema, right_rows) =
                    self.execute_set_query_body(right, outer_contexts, cte_store)?;

                if left_schema.columns.len() != right_schema.columns.len() {
                    return Err(Error::SetOperationColumnCountMismatch {
                        left_cols: left_schema.columns.len(),
                        right_cols: right_schema.columns.len(),
                    });
                }

                let combined_rows =
                    SetOperationOperator::apply(left_rows, right_rows, &left_schema, op)?;
                Ok((left_schema, combined_rows))
            }
        }
    }

    /// Executes a fully bound `BoundSelect` using reference nested-loop relational semantics.
    pub fn execute_bound_select(
        &mut self,
        bound: &BoundSelect,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<QueryResult> {
        // 1. Base table scan (pure full scan, no index optimization)
        let (raw_schema, initial_rows) = if let Some((cs, cr)) = cte_store.get(&bound.table_name) {
            (cs.clone(), cr.clone())
        } else {
            let mut table = self.txn.open_table(&bound.table_name)?;
            let s = table.schema().clone();
            let r = table.scan(None, None)?;
            (s, r)
        };

        let mut current_schema = if bound.joins.is_empty() {
            raw_schema
        } else {
            let qualifier = bound.table_alias.as_deref().unwrap_or(&bound.table_name);
            let cols = raw_schema
                .columns
                .iter()
                .map(|c| Column::new(format!("{qualifier}.{}", c.name), c.data_type, c.nullable))
                .collect();
            Schema::new_projected(cols)
        };
        let mut current_rows = initial_rows;

        // 2. Joins (nested-loop evaluation with 3VL condition semantics)
        for join in &bound.joins {
            let (right_raw_schema, right_rows) =
                if let Some((cs, cr)) = cte_store.get(&join.table_name) {
                    (cs.clone(), cr.clone())
                } else {
                    let mut table = self.txn.open_table(&join.table_name)?;
                    let s = table.schema().clone();
                    let r = table.scan(None, None)?;
                    (s, r)
                };

            let right_qualifier = join.table_alias.as_deref().unwrap_or(&join.table_name);
            let right_schema = Schema::new_projected(
                right_raw_schema
                    .columns
                    .iter()
                    .map(|c| {
                        Column::new(
                            format!("{right_qualifier}.{}", c.name),
                            c.data_type,
                            c.nullable,
                        )
                    })
                    .collect(),
            );

            // Combined schema for evaluating join condition
            let mut cond_cols = current_schema.columns.clone();
            cond_cols.extend(right_schema.columns.clone());
            let cond_schema = Schema::new_projected(cond_cols);

            let null_left = vec![Value::Null; current_schema.columns.len()];
            let null_right = vec![Value::Null; right_schema.columns.len()];

            match join.join_type {
                JoinType::Inner => {
                    let mut joined_rows = Vec::new();
                    for left_row in &current_rows {
                        for right_row in &right_rows {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                joined_rows.push(comb_row);
                            }
                        }
                    }
                    current_schema = cond_schema;
                    current_rows = joined_rows;
                }
                JoinType::Left => {
                    let mut joined_rows = Vec::new();
                    for left_row in &current_rows {
                        let mut matched = false;
                        for right_row in &right_rows {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                matched = true;
                                joined_rows.push(comb_row);
                            }
                        }
                        if !matched {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + null_right.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(null_right.clone());
                            joined_rows.push(Row::new(combined_vals));
                        }
                    }
                    current_schema = cond_schema;
                    current_rows = joined_rows;
                }
                JoinType::Right => {
                    let mut joined_rows = Vec::new();
                    for right_row in &right_rows {
                        let mut matched = false;
                        for left_row in &current_rows {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                matched = true;
                                joined_rows.push(comb_row);
                            }
                        }
                        if !matched {
                            let mut combined_vals =
                                Vec::with_capacity(null_left.len() + right_row.values.len());
                            combined_vals.extend(null_left.clone());
                            combined_vals.extend(right_row.values.clone());
                            joined_rows.push(Row::new(combined_vals));
                        }
                    }
                    current_schema = cond_schema;
                    current_rows = joined_rows;
                }
                JoinType::Full => {
                    let mut joined_rows = Vec::new();
                    let mut right_matched = vec![false; right_rows.len()];

                    for left_row in &current_rows {
                        let mut matched = false;
                        for (r_idx, right_row) in right_rows.iter().enumerate() {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                matched = true;
                                right_matched[r_idx] = true;
                                joined_rows.push(comb_row);
                            }
                        }
                        if !matched {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + null_right.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(null_right.clone());
                            joined_rows.push(Row::new(combined_vals));
                        }
                    }

                    for (r_idx, right_row) in right_rows.iter().enumerate() {
                        if !right_matched[r_idx] {
                            let mut combined_vals =
                                Vec::with_capacity(null_left.len() + right_row.values.len());
                            combined_vals.extend(null_left.clone());
                            combined_vals.extend(right_row.values.clone());
                            joined_rows.push(Row::new(combined_vals));
                        }
                    }
                    current_schema = cond_schema;
                    current_rows = joined_rows;
                }
                JoinType::Semi => {
                    let mut semijoin_rows = Vec::new();
                    for left_row in &current_rows {
                        let mut matched = false;
                        for right_row in &right_rows {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                matched = true;
                                break;
                            }
                        }
                        if matched {
                            semijoin_rows.push(left_row.clone());
                        }
                    }
                    current_rows = semijoin_rows;
                }
                JoinType::Anti => {
                    let mut antijoin_rows = Vec::new();
                    for left_row in &current_rows {
                        let mut matched = false;
                        for right_row in &right_rows {
                            let mut combined_vals =
                                Vec::with_capacity(left_row.values.len() + right_row.values.len());
                            combined_vals.extend(left_row.values.clone());
                            combined_vals.extend(right_row.values.clone());
                            let comb_row = Row::new(combined_vals);

                            let passes = self.eval_expr(
                                &join.on,
                                &comb_row,
                                &cond_schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            if passes == Value::Boolean(true) {
                                matched = true;
                                break;
                            }
                        }
                        if !matched {
                            antijoin_rows.push(left_row.clone());
                        }
                    }
                    current_rows = antijoin_rows;
                }
            }
        }

        // 3. Filter (WHERE clause) with 3VL
        if let Some(ref filter) = bound.filter {
            let mut filtered = Vec::new();
            for row in current_rows {
                let keep =
                    self.eval_expr(filter, &row, &current_schema, outer_contexts, cte_store)?;
                if keep == Value::Boolean(true) {
                    filtered.push(row);
                }
            }
            current_rows = filtered;
        }

        // 4. Aggregations and GROUP BY
        let mut working_schema = current_schema;
        let mut working_rows = current_rows;
        if !bound.group_by.is_empty() || !bound.aggregates.is_empty() {
            let (agg_schema, agg_rows) = AggregateOperator::apply(
                working_rows,
                &working_schema,
                &bound.group_by,
                &bound.aggregates,
            )?;
            working_schema = agg_schema;
            working_rows = agg_rows;

            // 5. HAVING filter
            if let Some(ref having) = bound.having {
                let mut having_filtered = Vec::new();
                for row in working_rows {
                    let keep =
                        self.eval_expr(having, &row, &working_schema, outer_contexts, cte_store)?;
                    if keep == Value::Boolean(true) {
                        having_filtered.push(row);
                    }
                }
                working_rows = having_filtered;
            }
        }

        // 6. Window functions
        if !bound.window_exprs.is_empty() {
            let (win_schema, win_rows) =
                WindowOperator::apply(working_rows, &working_schema, &bound.window_exprs)?;
            working_schema = win_schema;
            working_rows = win_rows;
        }

        // 7. Sort (ORDER BY)
        if !bound.order_by.is_empty() {
            working_rows = self.sort_rows(
                working_rows,
                &working_schema,
                &bound.order_by,
                outer_contexts,
                cte_store,
            )?;
        }

        // 8. Projection
        let (projected_schema, projected_rows) = self.project_rows(
            working_rows,
            &working_schema,
            &bound.projections,
            outer_contexts,
            cte_store,
        )?;

        // 9. Pagination (OFFSET / LIMIT)
        let final_rows = OffsetLimitOperator::apply(projected_rows, bound.offset, bound.limit);

        Ok(QueryResult::new(projected_schema, final_rows))
    }

    /// Evaluates an expression against a row in-memory, supporting subqueries recursively.
    pub fn eval_expr(
        &mut self,
        expr: &Expr,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Value> {
        self.txn
            .evaluate_expr_with_outer_internal(expr, row, schema, outer_contexts, cte_store)
    }

    fn sort_rows(
        &mut self,
        rows: Vec<Row>,
        schema: &Schema,
        sort_keys: &[SortKey],
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Vec<Row>> {
        // Pre-evaluate sort key values for each row to sort stably and deterministically
        let mut evaluated_keys: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut row_keys = Vec::with_capacity(sort_keys.len());
            for key in sort_keys {
                let val =
                    self.eval_expr(&key.expression, row, schema, outer_contexts, cte_store)?;
                row_keys.push(val);
            }
            evaluated_keys.push(row_keys);
        }

        let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
        let mut sort_error: Option<Error> = None;

        indexed.sort_by(|(idx_a, _), (idx_b, _)| {
            if sort_error.is_some() {
                return Ordering::Equal;
            }
            for (k_idx, key) in sort_keys.iter().enumerate() {
                let val_a = &evaluated_keys[*idx_a][k_idx];
                let val_b = &evaluated_keys[*idx_b][k_idx];
                match compare_values_for_sort(val_a, val_b, key.direction, key.nulls_first) {
                    Ok(Ordering::Equal) => {}
                    Ok(ord) => return ord,
                    Err(e) => {
                        sort_error = Some(e);
                        return Ordering::Equal;
                    }
                }
            }
            Ordering::Equal
        });

        if let Some(e) = sort_error {
            return Err(e);
        }

        Ok(indexed.into_iter().map(|(_, r)| r).collect())
    }

    fn project_rows(
        &mut self,
        rows: Vec<Row>,
        input_schema: &Schema,
        projections: &[(Expr, Option<String>)],
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(Schema, Vec<Row>)> {
        if projections.is_empty() {
            return Err(Error::InvalidProjection(
                "projection list cannot be empty".to_string(),
            ));
        }

        let mut out_columns = Vec::with_capacity(projections.len());
        for (idx, (expr, alias)) in projections.iter().enumerate() {
            let dt = expr.infer_type(input_schema)?;
            let name = match alias {
                Some(alias_name) => alias_name.clone(),
                None => match expr {
                    Expr::Column(col_name) => col_name.clone(),
                    _ => format!("col_{idx}"),
                },
            };
            let nullable = match expr {
                Expr::Column(col_name) => input_schema
                    .get_column(col_name)
                    .map(|c| c.nullable)
                    .unwrap_or(true),
                Expr::Literal(Value::Null) => true,
                _ => true,
            };
            out_columns.push(Column::new(name, dt, nullable));
        }

        let out_schema = Schema::new_projected(out_columns);
        let mut out_rows = Vec::with_capacity(rows.len());

        for row in rows {
            let mut values = Vec::with_capacity(projections.len());
            for (expr, _) in projections {
                values.push(self.eval_expr(expr, &row, input_schema, outer_contexts, cte_store)?);
            }
            out_rows.push(Row::new(values));
        }

        Ok((out_schema, out_rows))
    }
}
