use std::cmp::Ordering;
use std::fmt;

use crate::engine::types::{DataType, Row, Schema, Value};
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

/// Abstract Syntax Tree (AST) node for scalar expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Constant literal scalar value.
    Literal(Value),
    /// Column reference by name.
    Column(String),
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
            Expr::Column(name) => schema
                .get_column(name)
                .map(|c| c.data_type)
                .ok_or_else(|| Error::ColumnNotFound(name.clone())),
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
        }
    }

    /// Evaluates this expression against a single table `Row` and its `Schema`.
    ///
    /// Implements SQL-compliant 3-valued logic (3VL) for boolean operations and NULL handling.
    pub fn evaluate(&self, row: &Row, schema: &Schema) -> Result<Value> {
        match self {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Column(name) => {
                let idx = schema
                    .column_index(name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
                if idx >= row.values.len() {
                    return Err(Error::ColumnCountMismatch {
                        expected: schema.columns.len(),
                        found: row.values.len(),
                    });
                }
                Ok(row.values[idx].clone())
            }
            Expr::IsNull(inner) => {
                let val = inner.evaluate(row, schema)?;
                Ok(Value::Boolean(val.is_null()))
            }
            Expr::IsNotNull(inner) => {
                let val = inner.evaluate(row, schema)?;
                Ok(Value::Boolean(!val.is_null()))
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                let val = expr.evaluate(row, schema)?;
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
                    let l = left.evaluate(row, schema)?;
                    match l {
                        Value::Boolean(false) => Ok(Value::Boolean(false)),
                        Value::Boolean(true) => {
                            let r = right.evaluate(row, schema)?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = right.evaluate(row, schema)?;
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
                    let l = left.evaluate(row, schema)?;
                    match l {
                        Value::Boolean(true) => Ok(Value::Boolean(true)),
                        Value::Boolean(false) => {
                            let r = right.evaluate(row, schema)?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = right.evaluate(row, schema)?;
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
                    let l = left.evaluate(row, schema)?;
                    let r = right.evaluate(row, schema)?;

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
                    let l = left.evaluate(row, schema)?;
                    let r = right.evaluate(row, schema)?;

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
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Column(name) => write!(f, "{name}"),
            Expr::Binary { left, op, right } => write!(f, "({left} {op} {right})"),
            Expr::Unary { op, expr } => write!(f, "({op} {expr})"),
            Expr::IsNull(expr) => write!(f, "({expr} IS NULL)"),
            Expr::IsNotNull(expr) => write!(f, "({expr} IS NOT NULL)"),
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
