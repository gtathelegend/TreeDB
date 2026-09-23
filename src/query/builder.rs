use crate::engine::table::Table;
use crate::engine::types::{DataType, Row, Value};
use crate::error::Result;
use crate::query::expr::{BinaryOp, Expr};
use crate::query::operator::{
    FilterOperator, OffsetLimitOperator, ProjectOperator, QueryResult, SortDirection, SortKey,
    SortOperator,
};

/// Programmatic query builder for constructing and executing relational queries against a `Table`.
#[derive(Debug)]
pub struct Query<'a> {
    table: Table<'a>,
    filter: Option<Expr>,
    projection: Option<Vec<Expr>>,
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

    /// Specifies the projection expressions for the query output.
    pub fn project(mut self, expressions: Vec<Expr>) -> Self {
        self.projection = Some(expressions);
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
    /// 1. **Scan**: Evaluates whether primary-key point lookup or range scan optimization can be applied;
    ///    otherwise performs a full ordered table scan via B+ Tree index.
    /// 2. **Filter**: Applies predicate evaluation discarding false and NULL results.
    /// 3. **Sort**: Applies multi-key sorting if sort keys are present.
    /// 4. **Offset & Limit**: Applies row pagination.
    /// 5. **Project**: Evaluates projection expressions, generating the final `Schema` and `Row`s.
    pub fn execute(mut self) -> Result<QueryResult> {
        let schema = self.table.schema().clone();
        let pk_col_name = schema.primary_key_column().name.clone();
        let pk_type = schema.primary_key_column().data_type;

        // Determine scan plan based on filter predicate
        let scan_plan = match &self.filter {
            Some(predicate) => analyze_pk_predicate(predicate, &pk_col_name, pk_type),
            None => PkScanPlan::FullScan,
        };

        // Execute initial row retrieval
        let initial_rows: Vec<Row> = match scan_plan {
            PkScanPlan::PointLookup(val) => match self.table.get(val)? {
                Some(row) => vec![row],
                None => Vec::new(),
            },
            PkScanPlan::RangeScan { start, end } => self.table.scan(start, end)?,
            PkScanPlan::FullScan => self.table.scan(None, None)?,
        };

        // Stage 2: Filter Operator
        let filtered_rows = match &self.filter {
            Some(predicate) => FilterOperator::apply(initial_rows, &schema, predicate)?,
            None => initial_rows,
        };

        // Stage 3: Sort Operator
        let sorted_rows = SortOperator::apply(filtered_rows, &schema, &self.sort_keys)?;

        // Stage 4: Offset and Limit
        let paginated_rows = OffsetLimitOperator::apply(sorted_rows, self.offset, self.limit);

        // Stage 5: Project Operator
        match &self.projection {
            Some(projections) => {
                let (projected_schema, projected_rows) =
                    ProjectOperator::apply(paginated_rows, &schema, projections)?;
                Ok(QueryResult::new(projected_schema, projected_rows))
            }
            None => Ok(QueryResult::new(schema, paginated_rows)),
        }
    }
}

/// Internal execution plan for primary key scanning optimization.
#[derive(Debug, PartialEq)]
enum PkScanPlan<'a> {
    /// B+ Tree point lookup on a specific primary key value.
    PointLookup(&'a Value),
    /// B+ Tree range scan between `[start, end)`.
    RangeScan {
        start: Option<&'a Value>,
        end: Option<&'a Value>,
    },
    /// Full table scan over all primary keys.
    FullScan,
}

fn analyze_pk_predicate<'a>(
    predicate: &'a Expr,
    pk_col_name: &str,
    pk_type: DataType,
) -> PkScanPlan<'a> {
    match predicate {
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
        } => {
            if let Some(val) = extract_pk_col_literal(left, right, pk_col_name, pk_type) {
                return PkScanPlan::PointLookup(val);
            }
            PkScanPlan::FullScan
        }
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            if let PkScanPlan::PointLookup(val) = analyze_pk_predicate(left, pk_col_name, pk_type) {
                return PkScanPlan::PointLookup(val);
            }
            if let PkScanPlan::PointLookup(val) = analyze_pk_predicate(right, pk_col_name, pk_type)
            {
                return PkScanPlan::PointLookup(val);
            }

            let left_range = extract_single_bound(left, pk_col_name, pk_type);
            let right_range = extract_single_bound(right, pk_col_name, pk_type);

            match (left_range, right_range) {
                (Some((true, left_val)), Some((false, right_val))) => PkScanPlan::RangeScan {
                    start: Some(left_val),
                    end: Some(right_val),
                },
                (Some((false, left_val)), Some((true, right_val))) => PkScanPlan::RangeScan {
                    start: Some(right_val),
                    end: Some(left_val),
                },
                (Some((true, left_val)), None) => PkScanPlan::RangeScan {
                    start: Some(left_val),
                    end: None,
                },
                (Some((false, left_val)), None) => PkScanPlan::RangeScan {
                    start: None,
                    end: Some(left_val),
                },
                (None, Some((true, right_val))) => PkScanPlan::RangeScan {
                    start: Some(right_val),
                    end: None,
                },
                (None, Some((false, right_val))) => PkScanPlan::RangeScan {
                    start: None,
                    end: Some(right_val),
                },
                _ => PkScanPlan::FullScan,
            }
        }
        single_expr => {
            if let Some((is_start, val)) = extract_single_bound(single_expr, pk_col_name, pk_type) {
                if is_start {
                    PkScanPlan::RangeScan {
                        start: Some(val),
                        end: None,
                    }
                } else {
                    PkScanPlan::RangeScan {
                        start: None,
                        end: Some(val),
                    }
                }
            } else {
                PkScanPlan::FullScan
            }
        }
    }
}

fn extract_pk_col_literal<'a>(
    left: &'a Expr,
    right: &'a Expr,
    pk_col_name: &str,
    pk_type: DataType,
) -> Option<&'a Value> {
    match (left, right) {
        (Expr::Column(name), Expr::Literal(val)) if name == pk_col_name => {
            if val.data_type() == pk_type && !val.is_null() {
                Some(val)
            } else {
                None
            }
        }
        (Expr::Literal(val), Expr::Column(name)) if name == pk_col_name => {
            if val.data_type() == pk_type && !val.is_null() {
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
    pk_col_name: &str,
    pk_type: DataType,
) -> Option<(bool, &'a Value)> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::GtEq | BinaryOp::Gt,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column(name), Expr::Literal(val))
                if name == pk_col_name && val.data_type() == pk_type && !val.is_null() =>
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
                if name == pk_col_name && val.data_type() == pk_type && !val.is_null() =>
            {
                Some((true, val))
            }
            (Expr::Column(name), Expr::Literal(val))
                if name == pk_col_name && val.data_type() == pk_type && !val.is_null() =>
            {
                Some((false, val))
            }
            _ => None,
        },
        _ => None,
    }
}
