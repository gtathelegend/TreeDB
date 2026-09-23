use std::cmp::Ordering;

use crate::engine::types::{Column, DataType, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::Expr;

/// Result set returned by the execution of a query pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Logical schema describing the columns in each row of the result.
    pub schema: Schema,
    /// Ordered rows produced by the query.
    pub rows: Vec<Row>,
}

impl QueryResult {
    /// Creates a new `QueryResult` with the given schema and rows.
    pub fn new(schema: Schema, rows: Vec<Row>) -> Self {
        Self { schema, rows }
    }

    /// Returns the number of rows in the result set.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns `true` if the result set contains zero rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Returns a reference to the output schema.
    pub const fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns a slice of the output rows.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Consumes the result set and returns the vector of rows.
    pub fn into_rows(self) -> Vec<Row> {
        self.rows
    }

    /// Returns an iterator over the rows in the result set.
    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }
}

/// Sort direction for an `ORDER BY` sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDirection {
    /// Ascending order (lowest to highest).
    Ascending,
    /// Descending order (highest to lowest).
    Descending,
}

/// Sort specification defining an expression, sort direction, and NULL ordering policy.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// Evaluated scalar expression to sort by.
    pub expression: Expr,
    /// Ascending or Descending direction.
    pub direction: SortDirection,
    /// Explicit NULL placement: `Some(true)` for NULLs first, `Some(false)` for NULLs last,
    /// or `None` for the default (NULLs first in ASC, NULLs last in DESC).
    pub nulls_first: Option<bool>,
}

impl SortKey {
    /// Creates a new `SortKey` with the given expression and direction.
    pub fn new(expression: Expr, direction: SortDirection) -> Self {
        Self {
            expression,
            direction,
            nulls_first: None,
        }
    }

    /// Creates an ascending `SortKey`.
    pub fn asc(expression: Expr) -> Self {
        Self::new(expression, SortDirection::Ascending)
    }

    /// Creates a descending `SortKey`.
    pub fn desc(expression: Expr) -> Self {
        Self::new(expression, SortDirection::Descending)
    }

    /// Sets explicit NULL ordering preference.
    pub fn with_nulls_first(mut self, nulls_first: bool) -> Self {
        self.nulls_first = Some(nulls_first);
        self
    }
}

/// Compares two rows sequentially across multiple sort keys.
pub fn compare_rows_by_sort_keys(
    a: &Row,
    b: &Row,
    schema: &Schema,
    sort_keys: &[SortKey],
) -> Result<Ordering> {
    for key in sort_keys {
        let val_a = key.expression.evaluate(a, schema)?;
        let val_b = key.expression.evaluate(b, schema)?;

        let ord = compare_values_for_sort(&val_a, &val_b, key.direction, key.nulls_first)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// Compares two scalar values for sorting according to direction and NULL ordering rules.
pub fn compare_values_for_sort(
    a: &Value,
    b: &Value,
    direction: SortDirection,
    nulls_first: Option<bool>,
) -> Result<Ordering> {
    let effective_nulls_first = nulls_first.unwrap_or(direction == SortDirection::Ascending);

    match (a, b) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => {
            if effective_nulls_first {
                Ok(Ordering::Less)
            } else {
                Ok(Ordering::Greater)
            }
        }
        (_, Value::Null) => {
            if effective_nulls_first {
                Ok(Ordering::Greater)
            } else {
                Ok(Ordering::Less)
            }
        }
        (non_null_a, non_null_b) => {
            let base_ord = match (non_null_a, non_null_b) {
                (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
                (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
                (Value::UInt64(x), Value::UInt64(y)) => x.cmp(y),
                (Value::Float64(x), Value::Float64(y)) => x.total_cmp(y),
                (Value::String(x), Value::String(y)) => x.cmp(y),
                (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
                _ => {
                    return Err(Error::InvalidSortExpression(format!(
                        "cannot sort disparate types: {:?} vs {:?}",
                        non_null_a.data_type(),
                        non_null_b.data_type()
                    )))
                }
            };

            match direction {
                SortDirection::Ascending => Ok(base_ord),
                SortDirection::Descending => Ok(base_ord.reverse()),
            }
        }
    }
}

/// Logical filter operator that evaluates a boolean predicate against input rows.
pub struct FilterOperator;

impl FilterOperator {
    /// Applies a predicate expression to filter rows.
    ///
    /// Retains rows where the predicate evaluates to `Value::Boolean(true)`.
    /// Rows evaluating to `Value::Boolean(false)` or `Value::Null` are discarded.
    pub fn apply(rows: Vec<Row>, schema: &Schema, predicate: &Expr) -> Result<Vec<Row>> {
        let pred_type = predicate.infer_type(schema)?;
        if pred_type != DataType::Boolean && pred_type != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: pred_type.to_string(),
            });
        }

        let mut matched = Vec::new();
        for row in rows {
            let val = predicate.evaluate(&row, schema)?;
            if val == Value::Boolean(true) {
                matched.push(row);
            }
        }
        Ok(matched)
    }
}

/// Logical projection operator that transforms input rows into output rows.
pub struct ProjectOperator;

impl ProjectOperator {
    /// Applies a list of projection expressions to input rows, returning the projected `Schema` and `Row`s.
    pub fn apply(
        rows: Vec<Row>,
        input_schema: &Schema,
        projections: &[Expr],
    ) -> Result<(Schema, Vec<Row>)> {
        let named_projections: Vec<(Expr, Option<String>)> =
            projections.iter().cloned().map(|e| (e, None)).collect();
        Self::apply_named(rows, input_schema, &named_projections)
    }

    /// Applies a list of projection expressions with optional column aliases to input rows,
    /// returning the projected `Schema` and `Row`s.
    pub fn apply_named(
        rows: Vec<Row>,
        input_schema: &Schema,
        projections: &[(Expr, Option<String>)],
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
                Expr::Column(name) => input_schema
                    .get_column(name)
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
                values.push(expr.evaluate(&row, input_schema)?);
            }
            out_rows.push(Row::new(values));
        }

        Ok((out_schema, out_rows))
    }
}

/// Logical sort operator that orders input rows across one or more `SortKey`s.
pub struct SortOperator;

impl SortOperator {
    /// Sorts rows according to the specified sort keys.
    pub fn apply(mut rows: Vec<Row>, schema: &Schema, sort_keys: &[SortKey]) -> Result<Vec<Row>> {
        if sort_keys.is_empty() {
            return Ok(rows);
        }

        for key in sort_keys {
            key.expression.infer_type(schema)?;
        }

        let mut sort_err = Ok(());
        rows.sort_by(|a, b| {
            if sort_err.is_err() {
                return Ordering::Equal;
            }
            match compare_rows_by_sort_keys(a, b, schema, sort_keys) {
                Ok(ord) => ord,
                Err(e) => {
                    sort_err = Err(e);
                    Ordering::Equal
                }
            }
        });

        sort_err?;
        Ok(rows)
    }
}

/// Logical operator for pagination (OFFSET and LIMIT).
pub struct OffsetLimitOperator;

impl OffsetLimitOperator {
    /// Applies offset and limit pagination over rows.
    pub fn apply(rows: Vec<Row>, offset: Option<usize>, limit: Option<usize>) -> Vec<Row> {
        let skip = offset.unwrap_or(0);
        let take = limit.unwrap_or(usize::MAX);

        rows.into_iter().skip(skip).take(take).collect()
    }
}
