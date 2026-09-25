use std::cmp::Ordering;
use std::fmt;

use crate::engine::types::{Column, DataType, Row, Schema, Value};
use crate::error::{Error, Result};

/// Binary operators supported in query expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// Equality (`=`)
    Eq,
    /// Inequality (`!=`)
    NotEq,
    /// Less than (`<`)
    Lt,
    /// Less than or equal (`<=`)
    LtEq,
    /// Greater than (`>`)
    Gt,
    /// Greater than or equal (`>=`)
    GtEq,
    /// Logical AND (3-valued logic)
    And,
    /// Logical OR (3-valued logic)
    Or,
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryOp::Eq => write!(f, "="),
            BinaryOp::NotEq => write!(f, "!="),
            BinaryOp::Lt => write!(f, "<"),
            BinaryOp::LtEq => write!(f, "<="),
            BinaryOp::Gt => write!(f, ">"),
            BinaryOp::GtEq => write!(f, ">="),
            BinaryOp::And => write!(f, "AND"),
            BinaryOp::Or => write!(f, "OR"),
        }
    }
}

/// Unary operators supported in query expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Logical negation (`NOT`)
    Not,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnaryOp::Not => write!(f, "NOT"),
        }
    }
}

/// Aggregate functions supported in query expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFunction {
    /// Count rows or non-null values
    Count,
    /// Sum of numeric values
    Sum,
    /// Average of numeric values
    Avg,
    /// Minimum value
    Min,
    /// Maximum value
    Max,
}

impl fmt::Display for AggregateFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AggregateFunction::Count => write!(f, "COUNT"),
            AggregateFunction::Sum => write!(f, "SUM"),
            AggregateFunction::Avg => write!(f, "AVG"),
            AggregateFunction::Min => write!(f, "MIN"),
            AggregateFunction::Max => write!(f, "MAX"),
        }
    }
}

/// Aggregate expression representing an aggregation operation over a group of rows.
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateExpr {
    /// `COUNT(*)` counting all input rows including NULLs.
    CountAll,
    /// Standard aggregate function over an expression: `FUNC(expr)`.
    Function {
        /// Aggregate function kind.
        func: AggregateFunction,
        /// Inner argument expression.
        expr: Expr,
    },
}

impl AggregateExpr {
    /// Creates a `COUNT(*)` aggregate expression.
    pub fn count_all() -> Self {
        AggregateExpr::CountAll
    }

    /// Creates a `COUNT(expr)` aggregate expression.
    pub fn count(expr: Expr) -> Self {
        AggregateExpr::Function {
            func: AggregateFunction::Count,
            expr,
        }
    }

    /// Creates a `SUM(expr)` aggregate expression.
    pub fn sum(expr: Expr) -> Self {
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr,
        }
    }

    /// Creates an `AVG(expr)` aggregate expression.
    pub fn avg(expr: Expr) -> Self {
        AggregateExpr::Function {
            func: AggregateFunction::Avg,
            expr,
        }
    }

    /// Creates a `MIN(expr)` aggregate expression.
    pub fn min(expr: Expr) -> Self {
        AggregateExpr::Function {
            func: AggregateFunction::Min,
            expr,
        }
    }

    /// Creates a `MAX(expr)` aggregate expression.
    pub fn max(expr: Expr) -> Self {
        AggregateExpr::Function {
            func: AggregateFunction::Max,
            expr,
        }
    }

    /// Infers the output `DataType` of this aggregate expression.
    pub fn infer_type(&self, schema: &Schema) -> Result<DataType> {
        match self {
            AggregateExpr::CountAll => Ok(DataType::UInt64),
            AggregateExpr::Function { func, expr } => {
                let dt = expr.infer_type(schema)?;
                match func {
                    AggregateFunction::Count => Ok(DataType::UInt64),
                    AggregateFunction::Sum => match dt {
                        DataType::Int64 => Ok(DataType::Int64),
                        DataType::UInt64 => Ok(DataType::UInt64),
                        DataType::Float64 => Ok(DataType::Float64),
                        other => Err(Error::UnsupportedAggregateType {
                            func: "SUM".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                    AggregateFunction::Avg => match dt {
                        DataType::Int64 | DataType::UInt64 | DataType::Float64 => {
                            Ok(DataType::Float64)
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
                        | DataType::Bytes => Ok(dt),
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
                        | DataType::Bytes => Ok(dt),
                        other => Err(Error::UnsupportedAggregateType {
                            func: "MAX".to_string(),
                            data_type: other.to_string(),
                        }),
                    },
                }
            }
        }
    }

    /// Returns a canonical display name for the aggregate expression.
    pub fn display_name(&self) -> String {
        match self {
            AggregateExpr::CountAll => "COUNT(*)".to_string(),
            AggregateExpr::Function { func, expr } => format!("{func}({expr})"),
        }
    }
}

impl fmt::Display for AggregateExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// Bound window frame specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundWindowFrame {
    /// Frame start bound.
    pub start: crate::sql::ast::WindowFrameBound,
    /// Frame end bound.
    pub end: crate::sql::ast::WindowFrameBound,
}

impl fmt::Display for BoundWindowFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ROWS BETWEEN {} AND {}", self.start, self.end)
    }
}

/// Fully bound window specification.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundWindowSpec {
    /// Partitioning expressions.
    pub partition_by: Vec<Expr>,
    /// Ordering sort keys.
    pub order_by: Vec<crate::query::operator::SortKey>,
    /// Frame specification.
    pub frame: BoundWindowFrame,
}

impl fmt::Display for BoundWindowSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OVER (")?;
        let mut parts = Vec::new();
        if !self.partition_by.is_empty() {
            let p: Vec<String> = self.partition_by.iter().map(|e| e.to_string()).collect();
            parts.push(format!("PARTITION BY {}", p.join(", ")));
        }
        if !self.order_by.is_empty() {
            let o: Vec<String> = self
                .order_by
                .iter()
                .map(|k| format!("{} {:?}", k.expression, k.direction))
                .collect();
            parts.push(format!("ORDER BY {}", o.join(", ")));
        }
        parts.push(format!("{}", self.frame));
        write!(f, "{})", parts.join(" "))
    }
}

/// Fully bound window expression ready for physical window operator evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowExpr {
    /// Window function type.
    pub function: crate::sql::ast::WindowFunction,
    /// Function argument expressions.
    pub args: Vec<Expr>,
    /// Explicit non-negative offset for LAG/LEAD (default 1).
    pub offset: Option<usize>,
    /// Explicit default value expression for LAG/LEAD.
    pub default: Option<Expr>,
    /// Window specification.
    pub window_spec: BoundWindowSpec,
    /// Output data type.
    pub output_type: DataType,
    /// Canonical display / column name.
    pub display_name: String,
}

impl WindowExpr {
    /// Canonical display name for the window expression.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

impl fmt::Display for WindowExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

/// Result of pre-evaluating an uncorrelated subquery for reuse across row evaluations.
#[derive(Debug, Clone, PartialEq)]
pub enum MaterializedSubqueryResult {
    /// Scalar subquery resulting value (or NULL if 0 rows).
    Scalar(Value),
    /// IN subquery resulting set of column values.
    In(Vec<Value>),
    /// EXISTS subquery resulting boolean.
    Exists(bool),
}

/// Classification of subquery expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryKind {
    /// Scalar subquery returning at most one row and one column.
    Scalar {
        /// Inferred output data type of the single projected column.
        data_type: DataType,
    },
    /// IN subquery predicate: `expr IN (SELECT ...)` or `expr NOT IN (SELECT ...)`.
    In {
        /// Left-hand argument expression.
        expr: Box<Expr>,
        /// Whether the IN predicate is negated (`NOT IN`).
        negated: bool,
    },
    /// EXISTS subquery predicate: `EXISTS (SELECT ...)` or `NOT EXISTS (SELECT ...)`.
    Exists {
        /// Whether the EXISTS predicate is negated (`NOT EXISTS`).
        negated: bool,
    },
}

/// Fully bound and optimized subquery expression embedded in a physical expression tree.
#[derive(Debug, Clone, PartialEq)]
pub struct SubqueryExpr {
    /// Subquery form and semantics.
    pub kind: SubqueryKind,
    /// Parsed AST statement representing the subquery.
    pub statement: Box<crate::sql::ast::SelectStatement>,
    /// Optimized physical plan for executing the subquery.
    pub plan: crate::query::optimizer::PhysicalPlan,
    /// Whether the subquery references columns from enclosing outer scopes.
    pub is_correlated: bool,
    /// List of outer column names referenced by the subquery.
    pub outer_references: Vec<String>,
    /// Materialized result for uncorrelated subqueries (if pre-evaluated).
    pub materialized: Option<MaterializedSubqueryResult>,
}

/// Abstract Syntax Tree (AST) node for scalar expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Constant literal scalar value.
    Literal(Value),
    /// Column reference by name in the current scope.
    Column(String),
    /// Correlated column reference from an enclosing outer scope.
    OuterColumn {
        /// Name or qualified name of the outer column.
        name: String,
        /// Data type of the column.
        data_type: DataType,
        /// Scope depth (1 = immediate enclosing scope, 2 = outer-outer, etc.).
        scope_depth: usize,
    },
    /// Binary operation on two sub-expressions.
    Binary {
        /// Left-hand operand.
        left: Box<Expr>,
        /// Binary operator.
        op: BinaryOp,
        /// Right-hand operand.
        right: Box<Expr>,
    },
    /// Unary operation on a sub-expression.
    Unary {
        /// Unary operator.
        op: UnaryOp,
        /// Sub-expression operand.
        expr: Box<Expr>,
    },
    /// IS NULL predicate.
    IsNull(Box<Expr>),
    /// IS NOT NULL predicate.
    IsNotNull(Box<Expr>),
    /// IN list expression: `expr IN (item1, item2, ...)` or `expr NOT IN (...)`.
    InList {
        /// Left-hand argument expression.
        expr: Box<Expr>,
        /// List of item expressions.
        list: Vec<Expr>,
        /// Whether the predicate is negated (`NOT IN`).
        negated: bool,
    },
    /// Nested subquery expression.
    Subquery(Box<SubqueryExpr>),
}

/// Resolves a column within a schema, handling exact match, qualified name (e.g. `u.id`),
/// or unqualified name against a qualified schema (e.g. `id` against `users.id`).
pub fn resolve_column_in_schema<'a>(schema: &'a Schema, name: &str) -> Option<(usize, &'a Column)> {
    if let Some(idx) = schema.column_index(name) {
        return Some((idx, &schema.columns[idx]));
    }
    if let Some((_qual, col)) = name.split_once('.') {
        if let Some(idx) = schema.column_index(col) {
            return Some((idx, &schema.columns[idx]));
        }
    }
    let dot_name = format!(".{name}");
    let mut matches = Vec::new();
    for (idx, col) in schema.columns.iter().enumerate() {
        if col.name == name || col.name.ends_with(&dot_name) {
            matches.push((idx, col));
        }
    }
    if matches.len() == 1 {
        return Some(matches[0]);
    }
    None
}

impl Expr {
    /// Creates a literal expression from a `Value`.
    pub fn literal(value: impl Into<Value>) -> Self {
        Expr::Literal(value.into())
    }

    /// Creates a column reference expression.
    pub fn column(name: impl Into<String>) -> Self {
        Expr::Column(name.into())
    }

    /// Creates an equality comparison (`left = right`).
    pub fn eq(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::Eq,
            right: Box::new(right),
        }
    }

    /// Creates an inequality comparison (`left != right`).
    pub fn not_eq(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::NotEq,
            right: Box::new(right),
        }
    }

    /// Creates a less-than comparison (`left < right`).
    pub fn lt(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::Lt,
            right: Box::new(right),
        }
    }

    /// Creates a less-than-or-equal comparison (`left <= right`).
    pub fn lt_eq(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::LtEq,
            right: Box::new(right),
        }
    }

    /// Creates a greater-than comparison (`left > right`).
    pub fn gt(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::Gt,
            right: Box::new(right),
        }
    }

    /// Creates a greater-than-or-equal comparison (`left >= right`).
    pub fn gt_eq(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::GtEq,
            right: Box::new(right),
        }
    }

    /// Creates a logical AND conjunction (`left AND right`).
    pub fn and(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::And,
            right: Box::new(right),
        }
    }

    /// Creates a logical OR disjunction (`left OR right`).
    pub fn or(left: Expr, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::Or,
            right: Box::new(right),
        }
    }

    /// Creates a logical NOT negation (`NOT expr`).
    #[allow(clippy::should_implement_trait)]
    pub fn not(expr: Expr) -> Self {
        Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(expr),
        }
    }

    /// Creates an IS NULL check (`expr IS NULL`).
    pub fn is_null(expr: Expr) -> Self {
        Expr::IsNull(Box::new(expr))
    }

    /// Creates an IS NOT NULL check (`expr IS NOT NULL`).
    pub fn is_not_null(expr: Expr) -> Self {
        Expr::IsNotNull(Box::new(expr))
    }

    /// Infers the output `DataType` of this expression when evaluated against `schema`.
    ///
    /// Validates type compatibility statically before execution.
    pub fn infer_type(&self, schema: &Schema) -> Result<DataType> {
        match self {
            Expr::Literal(v) => Ok(v.data_type()),
            Expr::Column(name) => resolve_column_in_schema(schema, name)
                .map(|(_, c)| c.data_type)
                .ok_or_else(|| Error::ColumnNotFound(name.clone())),
            Expr::OuterColumn { data_type, .. } => Ok(*data_type),
            Expr::IsNull(_) | Expr::IsNotNull(_) => Ok(DataType::Boolean),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                let inner = expr.infer_type(schema)?;
                if inner == DataType::Boolean || inner == DataType::Null {
                    Ok(DataType::Boolean)
                } else {
                    Err(Error::InvalidBooleanExpression {
                        found: inner.to_string(),
                    })
                }
            }
            Expr::Binary { left, op, right } => {
                let lt = left.infer_type(schema)?;
                let rt = right.infer_type(schema)?;

                match op {
                    BinaryOp::And | BinaryOp::Or => {
                        let left_valid = lt == DataType::Boolean || lt == DataType::Null;
                        let right_valid = rt == DataType::Boolean || rt == DataType::Null;

                        if left_valid && right_valid {
                            Ok(DataType::Boolean)
                        } else {
                            let found = if !left_valid { lt } else { rt };
                            Err(Error::InvalidBooleanExpression {
                                found: found.to_string(),
                            })
                        }
                    }
                    BinaryOp::Eq | BinaryOp::NotEq => {
                        if lt == DataType::Null || rt == DataType::Null || lt == rt {
                            Ok(DataType::Boolean)
                        } else {
                            Err(Error::InvalidComparison {
                                left: lt.to_string(),
                                right: rt.to_string(),
                                op: op.to_string(),
                            })
                        }
                    }
                    BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                        if lt == DataType::Boolean || rt == DataType::Boolean {
                            return Err(Error::InvalidComparison {
                                left: lt.to_string(),
                                right: rt.to_string(),
                                op: op.to_string(),
                            });
                        }

                        if lt == DataType::Null || rt == DataType::Null || lt == rt {
                            Ok(DataType::Boolean)
                        } else {
                            Err(Error::InvalidComparison {
                                left: lt.to_string(),
                                right: rt.to_string(),
                                op: op.to_string(),
                            })
                        }
                    }
                }
            }
            Expr::InList { expr, list, .. } => {
                let lt = expr.infer_type(schema)?;
                for item in list {
                    let rt = item.infer_type(schema)?;
                    if lt != DataType::Null && rt != DataType::Null && lt != rt {
                        return Err(Error::InvalidComparison {
                            left: lt.to_string(),
                            right: rt.to_string(),
                            op: "IN".to_string(),
                        });
                    }
                }
                Ok(DataType::Boolean)
            }
            Expr::Subquery(subquery) => match &subquery.kind {
                SubqueryKind::Scalar { data_type } => Ok(*data_type),
                SubqueryKind::In { .. } | SubqueryKind::Exists { .. } => Ok(DataType::Boolean),
            },
        }
    }

    /// Evaluates this expression against a single table `Row` and its `Schema`.
    ///
    /// Implements SQL-compliant 3-valued logic (3VL) for boolean operations and NULL handling.
    pub fn evaluate(&self, row: &Row, schema: &Schema) -> Result<Value> {
        self.evaluate_with_outer(row, schema, &[])
    }

    /// Evaluates this expression against a single table `Row`, its `Schema`, and any active
    /// enclosing `outer_contexts` (for correlated subqueries and outer column resolution).
    pub fn evaluate_with_outer(
        &self,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<Value> {
        match self {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Column(name) => {
                let (idx, _) = resolve_column_in_schema(schema, name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
                if idx >= row.values.len() {
                    return Err(Error::ColumnCountMismatch {
                        expected: schema.columns.len(),
                        found: row.values.len(),
                    });
                }
                Ok(row.values[idx].clone())
            }
            Expr::OuterColumn {
                name, scope_depth, ..
            } => {
                if outer_contexts.is_empty()
                    || *scope_depth == 0
                    || *scope_depth > outer_contexts.len()
                {
                    return Err(Error::ColumnNotFound(format!(
                        "outer column '{name}' at depth {scope_depth} not available in execution context"
                    )));
                }
                let (outer_row, outer_schema) = outer_contexts[outer_contexts.len() - scope_depth];
                let (idx, _) = resolve_column_in_schema(outer_schema, name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
                if idx >= outer_row.values.len() {
                    return Err(Error::ColumnCountMismatch {
                        expected: outer_schema.columns.len(),
                        found: outer_row.values.len(),
                    });
                }
                Ok(outer_row.values[idx].clone())
            }
            Expr::IsNull(inner) => {
                let val = inner.evaluate_with_outer(row, schema, outer_contexts)?;
                Ok(Value::Boolean(val.is_null()))
            }
            Expr::IsNotNull(inner) => {
                let val = inner.evaluate_with_outer(row, schema, outer_contexts)?;
                Ok(Value::Boolean(!val.is_null()))
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                let val = expr.evaluate_with_outer(row, schema, outer_contexts)?;
                match val {
                    Value::Null => Ok(Value::Null),
                    Value::Boolean(b) => Ok(Value::Boolean(!b)),
                    other => Err(Error::InvalidBooleanExpression {
                        found: other.data_type().to_string(),
                    }),
                }
            }
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    let l = left.evaluate_with_outer(row, schema, outer_contexts)?;
                    match l {
                        Value::Boolean(false) => Ok(Value::Boolean(false)),
                        Value::Boolean(true) => {
                            let r = right.evaluate_with_outer(row, schema, outer_contexts)?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = right.evaluate_with_outer(row, schema, outer_contexts)?;
                            match r {
                                Value::Boolean(false) => Ok(Value::Boolean(false)),
                                Value::Boolean(true) | Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        other => Err(Error::InvalidBooleanExpression {
                            found: other.data_type().to_string(),
                        }),
                    }
                }
                BinaryOp::Or => {
                    let l = left.evaluate_with_outer(row, schema, outer_contexts)?;
                    match l {
                        Value::Boolean(true) => Ok(Value::Boolean(true)),
                        Value::Boolean(false) => {
                            let r = right.evaluate_with_outer(row, schema, outer_contexts)?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = right.evaluate_with_outer(row, schema, outer_contexts)?;
                            match r {
                                Value::Boolean(true) => Ok(Value::Boolean(true)),
                                Value::Boolean(false) | Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        other => Err(Error::InvalidBooleanExpression {
                            found: other.data_type().to_string(),
                        }),
                    }
                }
                BinaryOp::Eq | BinaryOp::NotEq => {
                    let l = left.evaluate_with_outer(row, schema, outer_contexts)?;
                    let r = right.evaluate_with_outer(row, schema, outer_contexts)?;

                    if l.is_null() || r.is_null() {
                        return Ok(Value::Null);
                    }

                    if l.data_type() != r.data_type() {
                        return Err(Error::InvalidComparison {
                            left: l.data_type().to_string(),
                            right: r.data_type().to_string(),
                            op: op.to_string(),
                        });
                    }

                    let is_eq = compare_values_eq(&l, &r);
                    let result = if *op == BinaryOp::Eq { is_eq } else { !is_eq };
                    Ok(Value::Boolean(result))
                }
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                    let l = left.evaluate_with_outer(row, schema, outer_contexts)?;
                    let r = right.evaluate_with_outer(row, schema, outer_contexts)?;

                    if l.is_null() || r.is_null() {
                        return Ok(Value::Null);
                    }

                    if l.data_type() == DataType::Boolean || r.data_type() == DataType::Boolean {
                        return Err(Error::InvalidComparison {
                            left: l.data_type().to_string(),
                            right: r.data_type().to_string(),
                            op: op.to_string(),
                        });
                    }

                    if l.data_type() != r.data_type() {
                        return Err(Error::InvalidComparison {
                            left: l.data_type().to_string(),
                            right: r.data_type().to_string(),
                            op: op.to_string(),
                        });
                    }

                    let ord = compare_values_ord(&l, &r, *op)?;
                    let res = match op {
                        BinaryOp::Lt => ord == Some(Ordering::Less),
                        BinaryOp::LtEq => {
                            ord == Some(Ordering::Less) || ord == Some(Ordering::Equal)
                        }
                        BinaryOp::Gt => ord == Some(Ordering::Greater),
                        BinaryOp::GtEq => {
                            ord == Some(Ordering::Greater) || ord == Some(Ordering::Equal)
                        }
                        _ => unreachable!(),
                    };

                    Ok(Value::Boolean(res))
                }
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let target = expr.evaluate_with_outer(row, schema, outer_contexts)?;
                let mut vals = Vec::with_capacity(list.len());
                for item in list {
                    vals.push(item.evaluate_with_outer(row, schema, outer_contexts)?);
                }
                Ok(eval_in_3vl(&target, &vals, *negated))
            }
            Expr::Subquery(subquery) => {
                if let Some(mat) = &subquery.materialized {
                    match mat {
                        MaterializedSubqueryResult::Scalar(val) => Ok(val.clone()),
                        MaterializedSubqueryResult::In(in_vals) => {
                            if let SubqueryKind::In { expr, negated } = &subquery.kind {
                                let target =
                                    expr.evaluate_with_outer(row, schema, outer_contexts)?;
                                Ok(eval_in_3vl(&target, in_vals, *negated))
                            } else {
                                unreachable!()
                            }
                        }
                        MaterializedSubqueryResult::Exists(b) => {
                            if let SubqueryKind::Exists { negated } = &subquery.kind {
                                let res = if *negated { !*b } else { *b };
                                Ok(Value::Boolean(res))
                            } else {
                                unreachable!()
                            }
                        }
                    }
                } else {
                    Err(Error::SqlBindingError(
                        "unmaterialized subquery encountered during direct expression evaluation without transaction runner"
                            .to_string(),
                    ))
                }
            }
        }
    }
}

/// Evaluates SQL 3-valued logic for `IN` and `NOT IN` predicates.
///
/// Rules:
/// - If `in_vals` is empty: `IN` returns `FALSE`, `NOT IN` returns `TRUE`.
/// - If `left_val` is `NULL`: returns `NULL`.
/// - If `left_val` matches any non-NULL value in `in_vals`: `IN` returns `TRUE`, `NOT IN` returns `FALSE`.
/// - If no match found:
///   - If `in_vals` contains any `NULL`: returns `NULL`.
///   - If `in_vals` has no `NULL`: `IN` returns `FALSE`, `NOT IN` returns `TRUE`.
pub fn eval_in_3vl(left_val: &Value, in_vals: &[Value], negated: bool) -> Value {
    if in_vals.is_empty() {
        return Value::Boolean(negated);
    }
    if left_val.is_null() {
        return Value::Null;
    }
    let mut has_null = false;
    for v in in_vals {
        if v.is_null() {
            has_null = true;
        } else if compare_values_eq(left_val, v) {
            return Value::Boolean(!negated);
        }
    }
    if has_null {
        Value::Null
    } else {
        Value::Boolean(negated)
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Column(name) => write!(f, "{name}"),
            Expr::OuterColumn { name, .. } => write!(f, "{name}"),
            Expr::Binary { left, op, right } => write!(f, "({left} {op} {right})"),
            Expr::Unary { op, expr } => write!(f, "({op} {expr})"),
            Expr::IsNull(expr) => write!(f, "({expr} IS NULL)"),
            Expr::IsNotNull(expr) => write!(f, "({expr} IS NOT NULL)"),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let items: Vec<String> = list.iter().map(|e| e.to_string()).collect();
                let not = if *negated { "NOT " } else { "" };
                write!(f, "({expr} {not}IN ({}))", items.join(", "))
            }
            Expr::Subquery(sub) => match &sub.kind {
                SubqueryKind::Scalar { .. } => write!(f, "({})", sub.statement),
                SubqueryKind::In { expr, negated } => {
                    let not = if *negated { "NOT " } else { "" };
                    write!(f, "({expr} {not}IN ({}))", sub.statement)
                }
                SubqueryKind::Exists { negated } => {
                    let not = if *negated { "NOT " } else { "" };
                    write!(f, "({not}EXISTS ({}))", sub.statement)
                }
            },
        }
    }
}

fn compare_values_eq(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Int64(a), Value::Int64(b)) => a == b,
        (Value::UInt64(a), Value::UInt64(b)) => a == b,
        (Value::Float64(a), Value::Float64(b)) => {
            // Standard IEEE-754: NaN == NaN is false
            if a.is_nan() || b.is_nan() {
                false
            } else {
                a == b
            }
        }
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bytes(a), Value::Bytes(b)) => a == b,
        _ => false,
    }
}

fn compare_values_ord(l: &Value, r: &Value, op: BinaryOp) -> Result<Option<Ordering>> {
    match (l, r) {
        (Value::Int64(a), Value::Int64(b)) => Ok(Some(a.cmp(b))),
        (Value::UInt64(a), Value::UInt64(b)) => Ok(Some(a.cmp(b))),
        (Value::Float64(a), Value::Float64(b)) => {
            // Standard IEEE-754 ordering: comparisons with NaN yield None (false for <, <=, >, >=)
            if a.is_nan() || b.is_nan() {
                Ok(None)
            } else {
                Ok(a.partial_cmp(b))
            }
        }
        (Value::String(a), Value::String(b)) => Ok(Some(a.cmp(b))),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(Some(a.cmp(b))),
        (Value::Boolean(_), Value::Boolean(_)) => Err(Error::InvalidComparison {
            left: "Boolean".to_string(),
            right: "Boolean".to_string(),
            op: op.to_string(),
        }),
        _ => Err(Error::InvalidComparison {
            left: l.data_type().to_string(),
            right: r.data_type().to_string(),
            op: op.to_string(),
        }),
    }
}

impl std::ops::Not for Expr {
    type Output = Self;

    fn not(self) -> Self::Output {
        Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(self),
        }
    }
}
