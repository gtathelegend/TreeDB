use std::cmp::Ordering;
use std::collections::HashMap;

use crate::engine::types::{Column, DataType, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, AggregateFunction, Expr};

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

/// Canonical typed representation of a scalar value suitable for robust in-memory grouping keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanonicalValue {
    Null,
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    Float64(u64),
    String(String),
    Bytes(Vec<u8>),
}

impl CanonicalValue {
    /// Maps a `Value` to its canonical representation.
    pub fn from_value(val: &Value) -> Self {
        match val {
            Value::Null => CanonicalValue::Null,
            Value::Boolean(b) => CanonicalValue::Boolean(*b),
            Value::Int64(i) => CanonicalValue::Int64(*i),
            Value::UInt64(u) => CanonicalValue::UInt64(*u),
            Value::Float64(f) => {
                if f.is_nan() {
                    CanonicalValue::Float64(f64::NAN.to_bits())
                } else if *f == 0.0 {
                    CanonicalValue::Float64((0.0f64).to_bits())
                } else {
                    CanonicalValue::Float64(f.to_bits())
                }
            }
            Value::String(s) => CanonicalValue::String(s.clone()),
            Value::Bytes(b) => CanonicalValue::Bytes(b.clone()),
        }
    }
}

/// Composite grouping key constructed from evaluated `GROUP BY` expressions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey(pub Vec<CanonicalValue>);

/// Internal accumulator state tracking incremental aggregate calculation for a single group.
#[derive(Debug, Clone)]
pub enum AggregateState {
    /// `COUNT(*)` accumulator.
    CountAll { count: u64 },
    /// `COUNT(expr)` accumulator (skips NULLs).
    Count { count: u64, expr: Expr },
    /// `SUM(Int64)` accumulator.
    SumInt64 { sum: i64, seen: bool, expr: Expr },
    /// `SUM(UInt64)` accumulator.
    SumUInt64 { sum: u64, seen: bool, expr: Expr },
    /// `SUM(Float64)` accumulator.
    SumFloat64 { sum: f64, seen: bool, expr: Expr },
    /// `AVG(numeric)` accumulator.
    Avg { sum: f64, count: u64, expr: Expr },
    /// `MIN(expr)` accumulator.
    Min { value: Option<Value>, expr: Expr },
    /// `MAX(expr)` accumulator.
    Max { value: Option<Value>, expr: Expr },
}

impl AggregateState {
    /// Initializes a new accumulator for the specified `AggregateExpr`.
    pub fn new(agg_expr: &AggregateExpr, schema: &Schema) -> Result<Self> {
        match agg_expr {
            AggregateExpr::CountAll => Ok(AggregateState::CountAll { count: 0 }),
            AggregateExpr::Function { func, expr } => {
                let dt = expr.infer_type(schema)?;
                match func {
                    AggregateFunction::Count => Ok(AggregateState::Count {
                        count: 0,
                        expr: expr.clone(),
                    }),
                    AggregateFunction::Sum => match dt {
                        DataType::Int64 => Ok(AggregateState::SumInt64 {
                            sum: 0,
                            seen: false,
                            expr: expr.clone(),
                        }),
                        DataType::UInt64 => Ok(AggregateState::SumUInt64 {
                            sum: 0,
                            seen: false,
                            expr: expr.clone(),
                        }),
                        DataType::Float64 => Ok(AggregateState::SumFloat64 {
                            sum: 0.0,
                            seen: false,
                            expr: expr.clone(),
                        }),
                        other => Err(Error::UnsupportedAggregateType {
                            func: "SUM".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                    AggregateFunction::Avg => match dt {
                        DataType::Int64 | DataType::UInt64 | DataType::Float64 => {
                            Ok(AggregateState::Avg {
                                sum: 0.0,
                                count: 0,
                                expr: expr.clone(),
                            })
                        }
                        other => Err(Error::UnsupportedAggregateType {
                            func: "AVG".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                    AggregateFunction::Min => match dt {
                        DataType::Boolean
                        | DataType::Int64
                        | DataType::UInt64
                        | DataType::Float64
                        | DataType::String
                        | DataType::Bytes => Ok(AggregateState::Min {
                            value: None,
                            expr: expr.clone(),
                        }),
                        other => Err(Error::UnsupportedAggregateType {
                            func: "MIN".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                    AggregateFunction::Max => match dt {
                        DataType::Boolean
                        | DataType::Int64
                        | DataType::UInt64
                        | DataType::Float64
                        | DataType::String
                        | DataType::Bytes => Ok(AggregateState::Max {
                            value: None,
                            expr: expr.clone(),
                        }),
                        other => Err(Error::UnsupportedAggregateType {
                            func: "MAX".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                }
            }
        }
    }

    /// Accumulates one row of input into this state.
    pub fn accumulate(&mut self, row: &Row, schema: &Schema) -> Result<()> {
        match self {
            AggregateState::CountAll { count } => {
                *count += 1;
                Ok(())
            }
            AggregateState::Count { count, expr } => {
                let val = expr.evaluate(row, schema)?;
                if !val.is_null() {
                    *count += 1;
                }
                Ok(())
            }
            AggregateState::SumInt64 { sum, seen, expr } => {
                let val = expr.evaluate(row, schema)?;
                match val {
                    Value::Int64(n) => {
                        *seen = true;
                        *sum = sum.checked_add(n).ok_or_else(|| {
                            Error::AggregateOverflow(format!(
                                "integer overflow adding {n} to current sum {sum}"
                            ))
                        })?;
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::UnsupportedAggregateType {
                            func: "SUM".to_string(),
                            data_type: other.data_type().to_string(),
                        });
                    }
                }
                Ok(())
            }
            AggregateState::SumUInt64 { sum, seen, expr } => {
                let val = expr.evaluate(row, schema)?;
                match val {
                    Value::UInt64(n) => {
                        *seen = true;
                        *sum = sum.checked_add(n).ok_or_else(|| {
                            Error::AggregateOverflow(format!(
                                "unsigned integer overflow adding {n} to current sum {sum}"
                            ))
                        })?;
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::UnsupportedAggregateType {
                            func: "SUM".to_string(),
                            data_type: other.data_type().to_string(),
                        });
                    }
                }
                Ok(())
            }
            AggregateState::SumFloat64 { sum, seen, expr } => {
                let val = expr.evaluate(row, schema)?;
                match val {
                    Value::Float64(f) => {
                        *seen = true;
                        *sum += f;
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::UnsupportedAggregateType {
                            func: "SUM".to_string(),
                            data_type: other.data_type().to_string(),
                        });
                    }
                }
                Ok(())
            }
            AggregateState::Avg { sum, count, expr } => {
                let val = expr.evaluate(row, schema)?;
                match val {
                    Value::Int64(n) => {
                        *count += 1;
                        *sum += n as f64;
                    }
                    Value::UInt64(n) => {
                        *count += 1;
                        *sum += n as f64;
                    }
                    Value::Float64(f) => {
                        *count += 1;
                        *sum += f;
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::UnsupportedAggregateType {
                            func: "AVG".to_string(),
                            data_type: other.data_type().to_string(),
                        });
                    }
                }
                Ok(())
            }
            AggregateState::Min { value, expr } => {
                let val = expr.evaluate(row, schema)?;
                if !val.is_null() {
                    match value {
                        None => *value = Some(val),
                        Some(cur) => {
                            let ord =
                                compare_values_for_sort(&val, cur, SortDirection::Ascending, None)?;
                            if ord == Ordering::Less {
                                *value = Some(val);
                            }
                        }
                    }
                }
                Ok(())
            }
            AggregateState::Max { value, expr } => {
                let val = expr.evaluate(row, schema)?;
                if !val.is_null() {
                    match value {
                        None => *value = Some(val),
                        Some(cur) => {
                            let ord =
                                compare_values_for_sort(&val, cur, SortDirection::Ascending, None)?;
                            if ord == Ordering::Greater {
                                *value = Some(val);
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Finalizes accumulator state into a scalar `Value`.
    pub fn finish(self) -> Value {
        match self {
            AggregateState::CountAll { count } => Value::UInt64(count),
            AggregateState::Count { count, .. } => Value::UInt64(count),
            AggregateState::SumInt64 { sum, seen, .. } => {
                if seen {
                    Value::Int64(sum)
                } else {
                    Value::Null
                }
            }
            AggregateState::SumUInt64 { sum, seen, .. } => {
                if seen {
                    Value::UInt64(sum)
                } else {
                    Value::Null
                }
            }
            AggregateState::SumFloat64 { sum, seen, .. } => {
                if seen {
                    Value::Float64(sum)
                } else {
                    Value::Null
                }
            }
            AggregateState::Avg { sum, count, .. } => {
                if count > 0 {
                    Value::Float64(sum / count as f64)
                } else {
                    Value::Null
                }
            }
            AggregateState::Min { value, .. } => value.unwrap_or(Value::Null),
            AggregateState::Max { value, .. } => value.unwrap_or(Value::Null),
        }
    }
}

/// Logical aggregation and grouping operator.
pub struct AggregateOperator;

impl AggregateOperator {
    /// Executes grouping and aggregation over input rows.
    ///
    /// Produces an intermediate Schema and Row set containing:
    /// - Grouping columns first: columns corresponding to each `group_by` expression.
    /// - Aggregate columns next: columns corresponding to each `aggregates` expression.
    pub fn apply(
        rows: Vec<Row>,
        input_schema: &Schema,
        group_by: &[Expr],
        aggregates: &[AggregateExpr],
    ) -> Result<(Schema, Vec<Row>)> {
        // Build intermediate output schema
        let mut out_cols = Vec::new();
        for (idx, grp_expr) in group_by.iter().enumerate() {
            let dt = grp_expr.infer_type(input_schema)?;
            let name = match grp_expr {
                Expr::Column(col_name) => col_name.clone(),
                _ => format!("_group_{idx}"),
            };
            out_cols.push(Column::new(name, dt, true));
        }

        for agg_expr in aggregates {
            let dt = agg_expr.infer_type(input_schema)?;
            let name = agg_expr.display_name();
            out_cols.push(Column::new(name, dt, true));
        }

        let out_schema = Schema::new_projected(out_cols);

        // Case 1: Global aggregation (no GROUP BY)
        if group_by.is_empty() {
            let mut accumulators = Vec::with_capacity(aggregates.len());
            for agg in aggregates {
                accumulators.push(AggregateState::new(agg, input_schema)?);
            }

            for row in &rows {
                for acc in &mut accumulators {
                    acc.accumulate(row, input_schema)?;
                }
            }

            let agg_values: Vec<Value> = accumulators.into_iter().map(|acc| acc.finish()).collect();
            return Ok((out_schema, vec![Row::new(agg_values)]));
        }

        // Case 2: Grouped aggregation (group_by is non-empty)
        if rows.is_empty() {
            return Ok((out_schema, Vec::new()));
        }

        let mut groups: HashMap<GroupKey, (Vec<Value>, Vec<AggregateState>)> = HashMap::new();
        let mut group_order: Vec<GroupKey> = Vec::new();

        for row in rows {
            let mut key_values = Vec::with_capacity(group_by.len());
            let mut canonical_keys = Vec::with_capacity(group_by.len());

            for grp_expr in group_by {
                let val = grp_expr.evaluate(&row, input_schema)?;
                canonical_keys.push(CanonicalValue::from_value(&val));
                key_values.push(val);
            }

            let key = GroupKey(canonical_keys);

            if let Some(entry) = groups.get_mut(&key) {
                for acc in &mut entry.1 {
                    acc.accumulate(&row, input_schema)?;
                }
            } else {
                group_order.push(key.clone());
                let mut initial_states = Vec::with_capacity(aggregates.len());
                for agg in aggregates {
                    initial_states.push(AggregateState::new(agg, input_schema)?);
                }
                for acc in &mut initial_states {
                    acc.accumulate(&row, input_schema)?;
                }
                groups.insert(key, (key_values, initial_states));
            }
        }

        let mut out_rows = Vec::with_capacity(group_order.len());
        for key in group_order {
            if let Some((grp_vals, accs)) = groups.remove(&key) {
                let mut row_values = grp_vals;
                for acc in accs {
                    row_values.push(acc.finish());
                }
                out_rows.push(Row::new(row_values));
            }
        }

        Ok((out_schema, out_rows))
    }
}
