use std::cmp::Ordering;
use std::collections::HashMap;

use crate::engine::types::{Column, DataType, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, AggregateFunction, Expr};
use crate::sql::ast::JoinType;

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

impl std::fmt::Display for SortDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SortDirection::Ascending => write!(f, "ASC"),
            SortDirection::Descending => write!(f, "DESC"),
        }
    }
}

impl std::fmt::Display for SortKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {}", self.expression, self.direction)?;
        if let Some(nf) = self.nulls_first {
            if nf {
                write!(f, " NULLS FIRST")?;
            } else {
                write!(f, " NULLS LAST")?;
            }
        }
        Ok(())
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

/// Relational join operator executing nested-loop joins over row streams.
pub struct JoinOperator;

impl JoinOperator {
    /// Executes a join between `left_rows` and `right_rows` using `join_type` and `on_predicate`
    /// with default join configuration.
    ///
    /// Returns the combined intermediate `Schema` and the resulting joined `Vec<Row>`.
    pub fn apply(
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: JoinType,
        on_predicate: &Expr,
    ) -> Result<(Schema, Vec<Row>)> {
        Self::apply_with_config(
            left_rows,
            left_schema,
            right_rows,
            right_schema,
            join_type,
            on_predicate,
            &crate::query::external_hash_join::JoinConfig::default(),
        )
        .map(|(s, r, _)| (s, r))
    }

    /// Executes a join between `left_rows` and `right_rows` using `join_type`, `on_predicate`,
    /// and the provided `JoinConfig`.
    ///
    /// If the `on_predicate` contains eligible equi-join conjuncts, executes hash join
    /// (in-memory or spilled depending on data volume and memory budget);
    /// otherwise falls back to nested-loop join.
    pub fn apply_with_config(
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: JoinType,
        on_predicate: &Expr,
        config: &crate::query::external_hash_join::JoinConfig,
    ) -> Result<(
        Schema,
        Vec<Row>,
        crate::query::external_hash_join::JoinExecutionMetrics,
    )> {
        if let Some(equi_plan) = crate::query::external_hash_join::analyze_equi_join(
            on_predicate,
            left_schema,
            right_schema,
        ) {
            let joiner = crate::query::external_hash_join::ExternalHashJoiner::new(
                left_schema,
                right_schema,
                join_type,
                equi_plan,
                config.clone(),
            );
            joiner.execute(left_rows, right_rows.to_vec())
        } else {
            if join_type == JoinType::Semi || join_type == JoinType::Anti {
                let mut combined_columns =
                    Vec::with_capacity(left_schema.columns.len() + right_schema.columns.len());
                for col in &left_schema.columns {
                    combined_columns.push(col.clone());
                }
                for col in &right_schema.columns {
                    combined_columns.push(col.clone());
                }
                let eval_schema = Schema::new_projected(combined_columns);

                let mut out_rows = Vec::new();
                let mut candidate_pairs = 0;
                let mut matched_rows = 0;
                let mut matched_outer_rows = 0;
                let mut unmatched_outer_rows = 0;

                for left_row in &left_rows {
                    let mut left_matched = false;
                    for right_row in right_rows {
                        candidate_pairs += 1;
                        let mut combined_values =
                            Vec::with_capacity(left_row.values.len() + right_row.values.len());
                        combined_values.extend_from_slice(&left_row.values);
                        combined_values.extend_from_slice(&right_row.values);
                        let joined_row = Row::new(combined_values);

                        let on_val = on_predicate.evaluate(&joined_row, &eval_schema)?;
                        if on_val == Value::Boolean(true) {
                            left_matched = true;
                            matched_rows += 1;
                            break;
                        }
                    }

                    if left_matched {
                        matched_outer_rows += 1;
                    } else {
                        unmatched_outer_rows += 1;
                    }

                    if (join_type == JoinType::Semi && left_matched)
                        || (join_type == JoinType::Anti && !left_matched)
                    {
                        out_rows.push(left_row.clone());
                    }
                }

                let metrics = crate::query::external_hash_join::JoinExecutionMetrics {
                    rows_left: left_rows.len(),
                    rows_right: right_rows.len(),
                    rows_output: out_rows.len(),
                    candidate_pairs,
                    matched_rows,
                    matched_outer_rows,
                    unmatched_outer_rows,
                    hash_build_rows: 0,
                    hash_probe_rows: 0,
                    spill_partitions: 0,
                    runs_created: 0,
                    bytes_spilled: 0,
                    partition_passes: 0,
                    peak_memory_bytes: 0,
                };

                return Ok((left_schema.clone(), out_rows, metrics));
            }

            let mut combined_columns =
                Vec::with_capacity(left_schema.columns.len() + right_schema.columns.len());

            for col in &left_schema.columns {
                let mut col_copy = col.clone();
                if join_type == JoinType::Right || join_type == JoinType::Full {
                    col_copy.nullable = true;
                }
                combined_columns.push(col_copy);
            }

            for col in &right_schema.columns {
                let mut col_copy = col.clone();
                if join_type == JoinType::Left || join_type == JoinType::Full {
                    col_copy.nullable = true;
                }
                combined_columns.push(col_copy);
            }

            let joined_schema = Schema::new_projected(combined_columns);
            let mut out_rows = Vec::new();
            let mut candidate_pairs = 0;
            let mut matched_rows = 0;
            let mut right_matched = vec![false; right_rows.len()];

            let left_nulls = vec![Value::Null; left_schema.columns.len()];
            let right_nulls = vec![Value::Null; right_schema.columns.len()];

            for left_row in &left_rows {
                let mut left_matched = false;

                for (r_idx, right_row) in right_rows.iter().enumerate() {
                    candidate_pairs += 1;
                    let mut combined_values =
                        Vec::with_capacity(left_row.values.len() + right_row.values.len());
                    combined_values.extend_from_slice(&left_row.values);
                    combined_values.extend_from_slice(&right_row.values);
                    let joined_row = Row::new(combined_values);

                    let on_val = on_predicate.evaluate(&joined_row, &joined_schema)?;
                    if on_val == Value::Boolean(true) {
                        left_matched = true;
                        right_matched[r_idx] = true;
                        matched_rows += 1;
                        out_rows.push(joined_row);
                    }
                }

                if !left_matched && (join_type == JoinType::Left || join_type == JoinType::Full) {
                    let mut combined_values =
                        Vec::with_capacity(left_row.values.len() + right_nulls.len());
                    combined_values.extend_from_slice(&left_row.values);
                    combined_values.extend_from_slice(&right_nulls);
                    out_rows.push(Row::new(combined_values));
                }
            }

            if join_type == JoinType::Right || join_type == JoinType::Full {
                for (r_idx, right_row) in right_rows.iter().enumerate() {
                    if !right_matched[r_idx] {
                        let mut combined_values =
                            Vec::with_capacity(left_nulls.len() + right_row.values.len());
                        combined_values.extend_from_slice(&left_nulls);
                        combined_values.extend_from_slice(&right_row.values);
                        out_rows.push(Row::new(combined_values));
                    }
                }
            }

            let metrics = crate::query::external_hash_join::JoinExecutionMetrics {
                rows_left: left_rows.len(),
                rows_right: right_rows.len(),
                rows_output: out_rows.len(),
                spill_partitions: 0,
                runs_created: 0,
                bytes_spilled: 0,
                partition_passes: 0,
                peak_memory_bytes: 0,
                candidate_pairs,
                matched_rows,
                matched_outer_rows: 0,
                unmatched_outer_rows: 0,
                hash_build_rows: 0,
                hash_probe_rows: 0,
            };

            Ok((joined_schema, out_rows, metrics))
        }
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
    /// Sorts rows according to the specified sort keys using the default sort configuration.
    pub fn apply(rows: Vec<Row>, schema: &Schema, sort_keys: &[SortKey]) -> Result<Vec<Row>> {
        Self::apply_with_config(
            rows,
            schema,
            sort_keys,
            &crate::query::external_sort::SortConfig::default(),
        )
        .map(|(r, _)| r)
    }

    /// Sorts rows according to the specified sort keys and configuration, returning the sorted rows
    /// and execution telemetry.
    pub fn apply_with_config(
        rows: Vec<Row>,
        schema: &Schema,
        sort_keys: &[SortKey],
        config: &crate::query::external_sort::SortConfig,
    ) -> Result<(Vec<Row>, crate::query::external_sort::SortExecutionMetrics)> {
        let sorter =
            crate::query::external_sort::ExternalSorter::new(schema, sort_keys, config.clone());
        sorter.sort(rows)
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// Converts this canonical representation back to a scalar `Value`.
    pub fn to_value(&self) -> Value {
        match self {
            CanonicalValue::Null => Value::Null,
            CanonicalValue::Boolean(b) => Value::Boolean(*b),
            CanonicalValue::Int64(i) => Value::Int64(*i),
            CanonicalValue::UInt64(u) => Value::UInt64(*u),
            CanonicalValue::Float64(bits) => Value::Float64(f64::from_bits(*bits)),
            CanonicalValue::String(s) => Value::String(s.clone()),
            CanonicalValue::Bytes(b) => Value::Bytes(b.clone()),
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

    /// Merges another accumulator state for the same aggregate expression into `self`.
    pub fn merge_from(&mut self, other: AggregateState) -> Result<()> {
        match (self, other) {
            (AggregateState::CountAll { count: c1 }, AggregateState::CountAll { count: c2 }) => {
                *c1 += c2;
                Ok(())
            }
            (AggregateState::Count { count: c1, .. }, AggregateState::Count { count: c2, .. }) => {
                *c1 += c2;
                Ok(())
            }
            (
                AggregateState::SumInt64 {
                    sum: s1,
                    seen: seen1,
                    ..
                },
                AggregateState::SumInt64 {
                    sum: s2,
                    seen: seen2,
                    ..
                },
            ) => {
                if seen2 {
                    if *seen1 {
                        *s1 = s1.checked_add(s2).ok_or_else(|| {
                            Error::AggregateOverflow(format!(
                                "integer overflow adding {s2} to current sum {s1}"
                            ))
                        })?;
                    } else {
                        *s1 = s2;
                        *seen1 = true;
                    }
                }
                Ok(())
            }
            (
                AggregateState::SumUInt64 {
                    sum: s1,
                    seen: seen1,
                    ..
                },
                AggregateState::SumUInt64 {
                    sum: s2,
                    seen: seen2,
                    ..
                },
            ) => {
                if seen2 {
                    if *seen1 {
                        *s1 = s1.checked_add(s2).ok_or_else(|| {
                            Error::AggregateOverflow(format!(
                                "unsigned integer overflow adding {s2} to current sum {s1}"
                            ))
                        })?;
                    } else {
                        *s1 = s2;
                        *seen1 = true;
                    }
                }
                Ok(())
            }
            (
                AggregateState::SumFloat64 {
                    sum: s1,
                    seen: seen1,
                    ..
                },
                AggregateState::SumFloat64 {
                    sum: s2,
                    seen: seen2,
                    ..
                },
            ) => {
                if seen2 {
                    if *seen1 {
                        *s1 += s2;
                    } else {
                        *s1 = s2;
                        *seen1 = true;
                    }
                }
                Ok(())
            }
            (
                AggregateState::Avg {
                    sum: s1, count: c1, ..
                },
                AggregateState::Avg {
                    sum: s2, count: c2, ..
                },
            ) => {
                *s1 += s2;
                *c1 += c2;
                Ok(())
            }
            (AggregateState::Min { value: v1, .. }, AggregateState::Min { value: v2, .. }) => {
                if let Some(val2) = v2 {
                    match v1 {
                        None => *v1 = Some(val2),
                        Some(val1) => {
                            let ord = compare_values_for_sort(
                                &val2,
                                val1,
                                SortDirection::Ascending,
                                None,
                            )?;
                            if ord == Ordering::Less {
                                *v1 = Some(val2);
                            }
                        }
                    }
                }
                Ok(())
            }
            (AggregateState::Max { value: v1, .. }, AggregateState::Max { value: v2, .. }) => {
                if let Some(val2) = v2 {
                    match v1 {
                        None => *v1 = Some(val2),
                        Some(val1) => {
                            let ord = compare_values_for_sort(
                                &val2,
                                val1,
                                SortDirection::Ascending,
                                None,
                            )?;
                            if ord == Ordering::Greater {
                                *v1 = Some(val2);
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => Err(Error::QueryExecutionError(
                "mismatched aggregate states during merge".to_string(),
            )),
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
    /// Executes grouping and aggregation over input rows using the default aggregation configuration.
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
        Self::apply_with_config(
            rows,
            input_schema,
            group_by,
            aggregates,
            &crate::query::external_aggregate::AggregateConfig::default(),
        )
        .map(|(s, r, _)| (s, r))
    }

    /// Executes grouping and aggregation over input rows using the provided `AggregateConfig`,
    /// returning the schema, result rows, and execution telemetry metrics.
    pub fn apply_with_config(
        rows: Vec<Row>,
        input_schema: &Schema,
        group_by: &[Expr],
        aggregates: &[AggregateExpr],
        config: &crate::query::external_aggregate::AggregateConfig,
    ) -> Result<(
        Schema,
        Vec<Row>,
        crate::query::external_aggregate::AggregateExecutionMetrics,
    )> {
        let aggregator = crate::query::external_aggregate::ExternalAggregator::new(
            input_schema,
            group_by,
            aggregates,
            config.clone(),
        );
        aggregator.aggregate(rows)
    }
}

/// Logical window function execution operator.
pub struct WindowOperator;

impl WindowOperator {
    /// Applies a sequence of window expressions to input rows, returning an updated `Schema`
    /// and `Row`s with appended window function calculation columns.
    pub fn apply(
        rows: Vec<Row>,
        input_schema: &Schema,
        window_exprs: &[crate::query::expr::WindowExpr],
    ) -> Result<(Schema, Vec<Row>)> {
        if window_exprs.is_empty() {
            return Ok((input_schema.clone(), rows));
        }

        let mut output_columns = input_schema.columns.clone();
        for we in window_exprs {
            output_columns.push(Column::new(we.display_name.clone(), we.output_type, true));
        }
        let output_schema = Schema::new_projected(output_columns);

        if rows.is_empty() {
            return Ok((output_schema, Vec::new()));
        }

        let mut all_computed_cols: Vec<Vec<Value>> = Vec::with_capacity(window_exprs.len());

        for we in window_exprs {
            let mut computed_col = vec![Value::Null; rows.len()];

            // Step 1: Partition rows into groups
            let mut partitions: HashMap<GroupKey, Vec<usize>> = HashMap::new();
            let mut partition_order: Vec<GroupKey> = Vec::new();

            for (row_idx, row) in rows.iter().enumerate() {
                let mut keys = Vec::with_capacity(we.window_spec.partition_by.len());
                for p_expr in &we.window_spec.partition_by {
                    let val = p_expr.evaluate(row, input_schema)?;
                    keys.push(CanonicalValue::from_value(&val));
                }
                let key = GroupKey(keys);
                if let Some(indices) = partitions.get_mut(&key) {
                    indices.push(row_idx);
                } else {
                    partition_order.push(key.clone());
                    partitions.insert(key, vec![row_idx]);
                }
            }

            // Step 2: Sort each partition and compute window values
            for key in &partition_order {
                let mut partition_indices = partitions.remove(key).unwrap();

                let mut sort_err: Result<()> = Ok(());
                if !we.window_spec.order_by.is_empty() {
                    partition_indices.sort_by(|&idx_a, &idx_b| {
                        if sort_err.is_err() {
                            return Ordering::Equal;
                        }
                        match compare_rows_by_sort_keys(
                            &rows[idx_a],
                            &rows[idx_b],
                            input_schema,
                            &we.window_spec.order_by,
                        ) {
                            Ok(Ordering::Equal) => idx_a.cmp(&idx_b),
                            Ok(ord) => ord,
                            Err(e) => {
                                sort_err = Err(e);
                                Ordering::Equal
                            }
                        }
                    });
                }
                sort_err?;

                let p_len = partition_indices.len();
                let mut prev_rank: u64 = 1;
                let mut prev_dense_rank: u64 = 1;

                for p in 0..p_len {
                    let orig_idx = partition_indices[p];
                    let val = match we.function {
                        crate::sql::ast::WindowFunction::RowNumber => Value::UInt64((p + 1) as u64),
                        crate::sql::ast::WindowFunction::Rank => {
                            if p == 0 {
                                prev_rank = 1;
                                Value::UInt64(1)
                            } else {
                                let is_peer = if we.window_spec.order_by.is_empty() {
                                    true
                                } else {
                                    compare_rows_by_sort_keys(
                                        &rows[partition_indices[p]],
                                        &rows[partition_indices[p - 1]],
                                        input_schema,
                                        &we.window_spec.order_by,
                                    )? == Ordering::Equal
                                };
                                if is_peer {
                                    Value::UInt64(prev_rank)
                                } else {
                                    let curr_rank = (p + 1) as u64;
                                    prev_rank = curr_rank;
                                    Value::UInt64(curr_rank)
                                }
                            }
                        }
                        crate::sql::ast::WindowFunction::DenseRank => {
                            if p == 0 {
                                prev_dense_rank = 1;
                                Value::UInt64(1)
                            } else {
                                let is_peer = if we.window_spec.order_by.is_empty() {
                                    true
                                } else {
                                    compare_rows_by_sort_keys(
                                        &rows[partition_indices[p]],
                                        &rows[partition_indices[p - 1]],
                                        input_schema,
                                        &we.window_spec.order_by,
                                    )? == Ordering::Equal
                                };
                                if is_peer {
                                    Value::UInt64(prev_dense_rank)
                                } else {
                                    prev_dense_rank += 1;
                                    Value::UInt64(prev_dense_rank)
                                }
                            }
                        }
                        crate::sql::ast::WindowFunction::Lag => {
                            let offset = we.offset.unwrap_or(1);
                            if offset <= p {
                                let target_idx = partition_indices[p - offset];
                                we.args[0].evaluate(&rows[target_idx], input_schema)?
                            } else if let Some(def_expr) = &we.default {
                                def_expr.evaluate(&rows[orig_idx], input_schema)?
                            } else {
                                Value::Null
                            }
                        }
                        crate::sql::ast::WindowFunction::Lead => {
                            let offset = we.offset.unwrap_or(1);
                            if p + offset < p_len {
                                let target_idx = partition_indices[p + offset];
                                we.args[0].evaluate(&rows[target_idx], input_schema)?
                            } else if let Some(def_expr) = &we.default {
                                def_expr.evaluate(&rows[orig_idx], input_schema)?
                            } else {
                                Value::Null
                            }
                        }
                        crate::sql::ast::WindowFunction::FirstValue => {
                            let start_idx = match we.window_spec.frame.start {
                                crate::sql::ast::WindowFrameBound::UnboundedPreceding => 0,
                                crate::sql::ast::WindowFrameBound::Preceding(n) => {
                                    p.saturating_sub(n as usize)
                                }
                                crate::sql::ast::WindowFrameBound::CurrentRow => p,
                                crate::sql::ast::WindowFrameBound::Following(n) => p + (n as usize),
                                crate::sql::ast::WindowFrameBound::UnboundedFollowing => {
                                    p_len.saturating_sub(1)
                                }
                            };
                            let end_idx = match we.window_spec.frame.end {
                                crate::sql::ast::WindowFrameBound::UnboundedPreceding => 0,
                                crate::sql::ast::WindowFrameBound::Preceding(n) => {
                                    p.saturating_sub(n as usize)
                                }
                                crate::sql::ast::WindowFrameBound::CurrentRow => p,
                                crate::sql::ast::WindowFrameBound::Following(n) => {
                                    (p + (n as usize)).min(p_len.saturating_sub(1))
                                }
                                crate::sql::ast::WindowFrameBound::UnboundedFollowing => {
                                    p_len.saturating_sub(1)
                                }
                            };

                            if start_idx > end_idx || start_idx >= p_len {
                                Value::Null
                            } else {
                                let target_idx = partition_indices[start_idx];
                                we.args[0].evaluate(&rows[target_idx], input_schema)?
                            }
                        }
                        crate::sql::ast::WindowFunction::LastValue => {
                            let start_idx = match we.window_spec.frame.start {
                                crate::sql::ast::WindowFrameBound::UnboundedPreceding => 0,
                                crate::sql::ast::WindowFrameBound::Preceding(n) => {
                                    p.saturating_sub(n as usize)
                                }
                                crate::sql::ast::WindowFrameBound::CurrentRow => p,
                                crate::sql::ast::WindowFrameBound::Following(n) => p + (n as usize),
                                crate::sql::ast::WindowFrameBound::UnboundedFollowing => {
                                    p_len.saturating_sub(1)
                                }
                            };
                            let end_idx = match we.window_spec.frame.end {
                                crate::sql::ast::WindowFrameBound::UnboundedPreceding => 0,
                                crate::sql::ast::WindowFrameBound::Preceding(n) => {
                                    p.saturating_sub(n as usize)
                                }
                                crate::sql::ast::WindowFrameBound::CurrentRow => p,
                                crate::sql::ast::WindowFrameBound::Following(n) => {
                                    (p + (n as usize)).min(p_len.saturating_sub(1))
                                }
                                crate::sql::ast::WindowFrameBound::UnboundedFollowing => {
                                    p_len.saturating_sub(1)
                                }
                            };

                            if start_idx > end_idx || start_idx >= p_len {
                                Value::Null
                            } else {
                                let target_idx = partition_indices[end_idx.min(p_len - 1)];
                                we.args[0].evaluate(&rows[target_idx], input_schema)?
                            }
                        }
                    };

                    computed_col[orig_idx] = val;
                }
            }

            all_computed_cols.push(computed_col);
        }

        let mut out_rows = Vec::with_capacity(rows.len());
        for (row_idx, row) in rows.into_iter().enumerate() {
            let mut values = row.values;
            for col in &all_computed_cols {
                values.push(col[row_idx].clone());
            }
            out_rows.push(Row::new(values));
        }

        Ok((output_schema, out_rows))
    }
}

fn coerce_row(row: Row, target_schema: &Schema) -> Result<Row> {
    let mut values = Vec::with_capacity(target_schema.columns.len());
    for (val, col) in row.values.into_iter().zip(&target_schema.columns) {
        let coerced = match (val, col.data_type) {
            (Value::Null, _) => Value::Null,
            (Value::Int64(i), DataType::UInt64) if i >= 0 => Value::UInt64(i as u64),
            (Value::Int64(i), DataType::Float64) => Value::Float64(i as f64),
            (Value::UInt64(u), DataType::Int64) if u <= i64::MAX as u64 => Value::Int64(u as i64),
            (Value::UInt64(u), DataType::Float64) => Value::Float64(u as f64),
            (v, _) => v,
        };
        values.push(coerced);
    }
    Ok(Row::new(values))
}

fn row_key(row: &Row) -> Vec<CanonicalValue> {
    row.values.iter().map(CanonicalValue::from_value).collect()
}

/// Physical in-memory execution operator for relational set operations.
pub struct SetOperationOperator;

impl SetOperationOperator {
    /// Applies a relational set operation on left and right input rows.
    pub fn apply(
        left_rows: Vec<Row>,
        right_rows: Vec<Row>,
        target_schema: &Schema,
        op: &crate::sql::SetOperator,
    ) -> Result<Vec<Row>> {
        let left_coerced: Vec<Row> = left_rows
            .into_iter()
            .map(|r| coerce_row(r, target_schema))
            .collect::<Result<Vec<_>>>()?;
        let right_coerced: Vec<Row> = right_rows
            .into_iter()
            .map(|r| coerce_row(r, target_schema))
            .collect::<Result<Vec<_>>>()?;

        match op {
            crate::sql::SetOperator::Union => {
                let mut seen = std::collections::HashSet::new();
                let mut result = Vec::new();
                for row in left_coerced {
                    let key = row_key(&row);
                    if seen.insert(key) {
                        result.push(row);
                    }
                }
                for row in right_coerced {
                    let key = row_key(&row);
                    if seen.insert(key) {
                        result.push(row);
                    }
                }
                Ok(result)
            }
            crate::sql::SetOperator::UnionAll => {
                let mut result = left_coerced;
                result.extend(right_coerced);
                Ok(result)
            }
            crate::sql::SetOperator::Intersect => {
                let mut right_set = std::collections::HashSet::new();
                for row in &right_coerced {
                    right_set.insert(row_key(row));
                }
                let mut emitted = std::collections::HashSet::new();
                let mut result = Vec::new();
                for row in left_coerced {
                    let key = row_key(&row);
                    if right_set.contains(&key) && emitted.insert(key) {
                        result.push(row);
                    }
                }
                Ok(result)
            }
            crate::sql::SetOperator::IntersectAll => {
                let mut right_counts: std::collections::HashMap<Vec<CanonicalValue>, usize> =
                    std::collections::HashMap::new();
                for row in &right_coerced {
                    let key = row_key(row);
                    *right_counts.entry(key).or_insert(0) += 1;
                }
                let mut result = Vec::new();
                for row in left_coerced {
                    let key = row_key(&row);
                    if let Some(cnt) = right_counts.get_mut(&key) {
                        if *cnt > 0 {
                            *cnt -= 1;
                            result.push(row);
                        }
                    }
                }
                Ok(result)
            }
            crate::sql::SetOperator::Except => {
                let mut right_set = std::collections::HashSet::new();
                for row in &right_coerced {
                    right_set.insert(row_key(row));
                }
                let mut emitted = std::collections::HashSet::new();
                let mut result = Vec::new();
                for row in left_coerced {
                    let key = row_key(&row);
                    if !right_set.contains(&key) && emitted.insert(key) {
                        result.push(row);
                    }
                }
                Ok(result)
            }
            crate::sql::SetOperator::ExceptAll => {
                let mut right_counts: std::collections::HashMap<Vec<CanonicalValue>, usize> =
                    std::collections::HashMap::new();
                for row in &right_coerced {
                    let key = row_key(row);
                    *right_counts.entry(key).or_insert(0) += 1;
                }
                let mut result = Vec::new();
                for row in left_coerced {
                    let key = row_key(&row);
                    if let Some(cnt) = right_counts.get_mut(&key) {
                        if *cnt > 0 {
                            *cnt -= 1;
                            continue;
                        }
                    }
                    result.push(row);
                }
                Ok(result)
            }
        }
    }
}
