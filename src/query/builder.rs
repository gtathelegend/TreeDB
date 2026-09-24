use crate::engine::table::Table;
use crate::engine::types::{DataType, Row, Value};
use crate::error::Result;
use crate::query::expr::{AggregateExpr, BinaryOp, Expr};
use crate::query::operator::{
    AggregateOperator, FilterOperator, OffsetLimitOperator, ProjectOperator, QueryResult,
    SortDirection, SortKey, SortOperator,
};

/// Programmatic query builder for constructing and executing relational queries against a `Table`.
#[derive(Debug)]
pub struct Query<'a> {
    table: Table<'a>,
    filter: Option<Expr>,
    group_by: Vec<Expr>,
    aggregates: Vec<AggregateExpr>,
    having: Option<Expr>,
    projection: Option<Vec<(Expr, Option<String>)>>,
    sort_keys: Vec<SortKey>,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl<'a> Query<'a> {
    /// Creates a new `Query` builder for the specified `Table`.
    pub fn new(table: Table<'a>) -> Self {
        Self {
            table,
            filter: None,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            having: None,
            projection: None,
            sort_keys: Vec::new(),
            offset: None,
            limit: None,
        }
    }

    /// Adds a filtering predicate expression to the query.
    ///
    /// If a filter is already defined, the new predicate is combined using logical `AND`.
    pub fn filter(mut self, predicate: Expr) -> Self {
        self.filter = match self.filter {
            Some(existing) => Some(Expr::and(existing, predicate)),
            None => Some(predicate),
        };
        self
    }

    /// Adds grouping expressions to the query (`GROUP BY`).
    pub fn group_by(mut self, exprs: Vec<Expr>) -> Self {
        self.group_by.extend(exprs);
        self
    }

    /// Appends aggregate expressions to evaluate.
    pub fn aggregate(mut self, aggregates: Vec<AggregateExpr>) -> Self {
        self.aggregates.extend(aggregates);
        self
    }

    /// Adds a post-aggregation filtering predicate expression (`HAVING`).
    pub fn having(mut self, predicate: Expr) -> Self {
        self.having = match self.having {
            Some(existing) => Some(Expr::and(existing, predicate)),
            None => Some(predicate),
        };
        self
    }

    /// Specifies the projection expressions for the query output.
    pub fn project(self, expressions: Vec<Expr>) -> Self {
        let named: Vec<(Expr, Option<String>)> =
            expressions.into_iter().map(|e| (e, None)).collect();
        self.project_with_aliases(named)
    }

    /// Specifies the projection expressions with optional output column aliases.
    pub fn project_with_aliases(mut self, projections: Vec<(Expr, Option<String>)>) -> Self {
        self.projection = Some(projections);
        self
    }

    /// Adds a single sort key with expression and sort direction.
    pub fn sort_by(mut self, expr: Expr, direction: SortDirection) -> Self {
        self.sort_keys.push(SortKey::new(expr, direction));
        self
    }

    /// Appends multiple sort keys to the query.
    pub fn sort(mut self, keys: Vec<SortKey>) -> Self {
        self.sort_keys.extend(keys);
        self
    }

    /// Sets the number of rows to skip (OFFSET).
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets the maximum number of rows to return (LIMIT).
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Returns a reference to the underlying `Table`.
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// Returns a mutable reference to the underlying `Table`.
    pub fn table_mut(&mut self) -> &mut Table<'a> {
        &mut self.table
    }

    /// Executes the query pipeline and returns the resulting `QueryResult`.
    ///
    /// # Pipeline Stages
    /// 1. **Scan**: Evaluates access paths and fetches initial candidate rows.
    /// 2. **Filter**: Evaluates WHERE predicate, discarding false and NULL results.
    /// 3. **GROUP BY & Aggregation**: Groups rows and evaluates accumulators.
    /// 4. **HAVING**: Evaluates post-aggregation filter predicates.
    /// 5. **Sort**: Orders rows across sort keys.
    /// 6. **Project**: Evaluates output projection expressions.
    /// 7. **Offset & Limit**: Applies row pagination.
    pub fn execute(mut self) -> Result<QueryResult> {
        let schema = self.table.schema().clone();
        let pk_col_name = schema.primary_key_column().name.clone();
        let pk_type = schema.primary_key_column().data_type;

        // 1. Check Primary Key Scan Plan
        let pk_plan = match &self.filter {
            Some(predicate) => analyze_col_predicate(predicate, &pk_col_name, pk_type),
            None => ColScanPlan::FullScan,
        };

        // 2. Select Access Path
        let initial_rows: Vec<Row> = match pk_plan {
            ColScanPlan::PointLookup(val) => match self.table.get(val)? {
                Some(row) => vec![row],
                None => Vec::new(),
            },
            ColScanPlan::RangeScan { start, end } => self.table.scan(start, end)?,
            ColScanPlan::FullScan => {
                // Check if any secondary index can be used
                let mut sec_plan: Option<(String, ColScanPlan)> = None;
                if let Some(predicate) = &self.filter {
                    for (meta, _) in self.table.indexes() {
                        let col = &schema.columns[meta.column_idx];
                        let plan = analyze_col_predicate(predicate, &col.name, col.data_type);
                        if !matches!(plan, ColScanPlan::FullScan) {
                            sec_plan = Some((meta.name.clone(), plan));
                            break;
                        }
                    }
                }

                match sec_plan {
                    Some((idx_name, ColScanPlan::PointLookup(val))) => {
                        self.table.index_lookup(&idx_name, val)?
                    }
                    Some((idx_name, ColScanPlan::RangeScan { start, end })) => {
                        self.table.index_scan(&idx_name, start, end)?
                    }
                    _ => self.table.scan(None, None)?,
                }
            }
        };

        // Stage 2: Filter Operator (Complete WHERE predicate evaluation)
        let filtered_rows = match &self.filter {
            Some(predicate) => FilterOperator::apply(initial_rows, &schema, predicate)?,
            None => initial_rows,
        };

        // Stage 3 & 4: Aggregation and HAVING
        let is_aggregate_query = !self.group_by.is_empty() || !self.aggregates.is_empty();

        if is_aggregate_query {
            let (grouped_schema, grouped_rows) =
                AggregateOperator::apply(filtered_rows, &schema, &self.group_by, &self.aggregates)?;

            let having_rows = match &self.having {
                Some(predicate) => FilterOperator::apply(grouped_rows, &grouped_schema, predicate)?,
                None => grouped_rows,
            };

            let sorted_rows = SortOperator::apply(having_rows, &grouped_schema, &self.sort_keys)?;

            match &self.projection {
                Some(projections) => {
                    let (projected_schema, projected_rows) =
                        ProjectOperator::apply_named(sorted_rows, &grouped_schema, projections)?;
                    let paginated_rows =
                        OffsetLimitOperator::apply(projected_rows, self.offset, self.limit);
                    Ok(QueryResult::new(projected_schema, paginated_rows))
                }
                None => {
                    let paginated_rows =
                        OffsetLimitOperator::apply(sorted_rows, self.offset, self.limit);
                    Ok(QueryResult::new(grouped_schema, paginated_rows))
                }
            }
        } else {
            let sorted_rows = SortOperator::apply(filtered_rows, &schema, &self.sort_keys)?;

            match &self.projection {
                Some(projections) => {
                    let (projected_schema, projected_rows) =
                        ProjectOperator::apply_named(sorted_rows, &schema, projections)?;
                    let paginated_rows =
                        OffsetLimitOperator::apply(projected_rows, self.offset, self.limit);
                    Ok(QueryResult::new(projected_schema, paginated_rows))
                }
                None => {
                    let paginated_rows =
                        OffsetLimitOperator::apply(sorted_rows, self.offset, self.limit);
                    Ok(QueryResult::new(schema, paginated_rows))
                }
            }
        }
    }
}

/// Internal execution plan for column-based scanning optimization.
#[derive(Debug, PartialEq)]
enum ColScanPlan<'a> {
    /// Point lookup on a specific column value.
    PointLookup(&'a Value),
    /// Range scan between `[start, end)`.
    RangeScan {
        start: Option<&'a Value>,
        end: Option<&'a Value>,
    },
    /// Full scan over all rows.
    FullScan,
}

fn analyze_col_predicate<'a>(
    predicate: &'a Expr,
    col_name: &str,
    col_type: DataType,
) -> ColScanPlan<'a> {
    match predicate {
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
        } => {
            if let Some(val) = extract_col_literal(left, right, col_name, col_type) {
                return ColScanPlan::PointLookup(val);
            }
            ColScanPlan::FullScan
        }
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            if let ColScanPlan::PointLookup(val) = analyze_col_predicate(left, col_name, col_type) {
                return ColScanPlan::PointLookup(val);
            }
            if let ColScanPlan::PointLookup(val) = analyze_col_predicate(right, col_name, col_type)
            {
                return ColScanPlan::PointLookup(val);
            }

            let left_range = extract_single_bound(left, col_name, col_type);
            let right_range = extract_single_bound(right, col_name, col_type);

            match (left_range, right_range) {
                (Some((true, left_val)), Some((false, right_val))) => ColScanPlan::RangeScan {
                    start: Some(left_val),
                    end: Some(right_val),
                },
                (Some((false, left_val)), Some((true, right_val))) => ColScanPlan::RangeScan {
                    start: Some(right_val),
                    end: Some(left_val),
                },
                (Some((true, left_val)), None) => ColScanPlan::RangeScan {
                    start: Some(left_val),
                    end: None,
                },
                (Some((false, left_val)), None) => ColScanPlan::RangeScan {
                    start: None,
                    end: Some(left_val),
                },
                (None, Some((true, right_val))) => ColScanPlan::RangeScan {
                    start: Some(right_val),
                    end: None,
                },
                (None, Some((false, right_val))) => ColScanPlan::RangeScan {
                    start: None,
                    end: Some(right_val),
                },
                _ => ColScanPlan::FullScan,
            }
        }
        single_expr => {
            if let Some((is_start, val)) = extract_single_bound(single_expr, col_name, col_type) {
                if is_start {
                    ColScanPlan::RangeScan {
                        start: Some(val),
                        end: None,
                    }
                } else {
                    ColScanPlan::RangeScan {
                        start: None,
                        end: Some(val),
                    }
                }
            } else {
                ColScanPlan::FullScan
            }
        }
    }
}

fn extract_col_literal<'a>(
    left: &'a Expr,
    right: &'a Expr,
    col_name: &str,
    col_type: DataType,
) -> Option<&'a Value> {
    match (left, right) {
        (Expr::Column(name), Expr::Literal(val)) if name == col_name => {
            if val.data_type() == col_type || val.is_null() {
                Some(val)
            } else {
                None
            }
        }
        (Expr::Literal(val), Expr::Column(name)) if name == col_name => {
            if val.data_type() == col_type || val.is_null() {
                Some(val)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_single_bound<'a>(
    expr: &'a Expr,
    col_name: &str,
    col_type: DataType,
) -> Option<(bool, &'a Value)> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::GtEq | BinaryOp::Gt,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column(name), Expr::Literal(val))
                if name == col_name && (val.data_type() == col_type || val.is_null()) =>
            {
                Some((true, val))
            }
            _ => None,
        },
        Expr::Binary {
            left,
            op: BinaryOp::LtEq | BinaryOp::Lt,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Literal(val), Expr::Column(name))
                if name == col_name && (val.data_type() == col_type || val.is_null()) =>
            {
                Some((true, val))
            }
            (Expr::Column(name), Expr::Literal(val))
                if name == col_name && (val.data_type() == col_type || val.is_null()) =>
            {
                Some((false, val))
            }
            _ => None,
        },
        _ => None,
    }
}
