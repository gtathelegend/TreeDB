use crate::engine::types::{DataType, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{BinaryOp, Expr, UnaryOp};
use crate::query::operator::{SortDirection, SortKey};
use crate::sql::ast::{
    OrderDirection, SelectItem, SelectStatement, SqlBinaryOp, SqlExpr, SqlLiteral, SqlUnaryOp,
};

/// Semantic binding context holding the target table schema and name/alias mappings.
#[derive(Debug, Clone)]
pub struct BindingContext<'a> {
    /// Canonical table name from the `FROM` clause.
    pub table_name: &'a str,
    /// Optional table alias from the `FROM` clause (`FROM table AS alias`).
    pub table_alias: Option<&'a str>,
    /// Logical schema of the referenced table.
    pub schema: &'a Schema,
}

impl<'a> BindingContext<'a> {
    /// Creates a new `BindingContext`.
    pub fn new(table_name: &'a str, table_alias: Option<&'a str>, schema: &'a Schema) -> Self {
        Self {
            table_name,
            table_alias,
            schema,
        }
    }

    /// Verifies if a given table or alias qualifier is valid in this binding context.
    pub fn validate_qualifier(&self, qualifier: &str) -> Result<()> {
        if self.table_name == qualifier || self.table_alias == Some(qualifier) {
            Ok(())
        } else {
            Err(Error::SqlBindingError(format!(
                "table or alias '{qualifier}' not found in FROM clause (expected '{}')",
                self.table_alias.unwrap_or(self.table_name)
            )))
        }
    }
}

/// A fully validated, bound SQL query ready for execution against TreeDB's query engine.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// Target table name to query.
    pub table_name: String,
    /// Output projection expressions with optional output column aliases.
    pub projections: Vec<(Expr, Option<String>)>,
    /// Optional boolean filtering predicate expression.
    pub filter: Option<Expr>,
    /// Ordered list of sort keys.
    pub order_by: Vec<SortKey>,
    /// Maximum rows to return (LIMIT).
    pub limit: Option<usize>,
    /// Number of rows to skip (OFFSET).
    pub offset: Option<usize>,
}

/// Binds a parsed `SelectStatement` AST against a table's `Schema`.
///
/// Performs name resolution for columns, expands wildcards (`*`, `table.*`),
/// validates qualifiers and aliases, performs static type inference, and
/// translates AST expressions into executable Phase 14 `Expr`s.
pub fn bind_select(statement: &SelectStatement, schema: &Schema) -> Result<BoundSelect> {
    let ctx = BindingContext::new(
        &statement.from.name,
        statement.from.alias.as_deref(),
        schema,
    );

    // 1. Bind and expand projections
    let mut projections = Vec::new();
    for item in &statement.projection {
        match item {
            SelectItem::Wildcard => {
                for col in &schema.columns {
                    projections.push((Expr::Column(col.name.clone()), None));
                }
            }
            SelectItem::QualifiedWildcard(qualifier) => {
                ctx.validate_qualifier(qualifier)?;
                for col in &schema.columns {
                    projections.push((Expr::Column(col.name.clone()), None));
                }
            }

            SelectItem::Expression { expr, alias } => {
                let bound = bind_expr(expr, &ctx)?;
                let _ = bound.infer_type(schema)?;
                projections.push((bound, alias.clone()));
            }
        }
    }

    if projections.is_empty() {
        return Err(Error::InvalidProjection(
            "projection list cannot be empty".to_string(),
        ));
    }

    // 2. Bind WHERE filtering predicate
    let filter = if let Some(selection) = &statement.selection {
        let bound = bind_expr(selection, &ctx)?;
        let dt = bound.infer_type(schema)?;
        if dt != DataType::Boolean && dt != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: dt.to_string(),
            });
        }
        Some(bound)
    } else {
        None
    };

    // 3. Bind ORDER BY sort expressions
    let mut order_by = Vec::with_capacity(statement.order_by.len());
    for order_item in &statement.order_by {
        let bound_expr = bind_expr(&order_item.expr, &ctx)?;
        let _ = bound_expr.infer_type(schema)?;
        let direction = match order_item.direction {
            OrderDirection::Asc => SortDirection::Ascending,
            OrderDirection::Desc => SortDirection::Descending,
        };
        order_by.push(SortKey::new(bound_expr, direction));
    }

    // 4. Limit and Offset
    let limit = statement.limit.map(|v| v as usize);
    let offset = statement.offset.map(|v| v as usize);

    Ok(BoundSelect {
        table_name: statement.from.name.clone(),
        projections,
        filter,
        order_by,
        limit,
        offset,
    })
}

/// Recursively binds a `SqlExpr` into a Phase 14 `Expr`.
pub fn bind_expr(expr: &SqlExpr, ctx: &BindingContext<'_>) -> Result<Expr> {
    match expr {
        SqlExpr::Literal(lit) => match lit {
            SqlLiteral::Null => Ok(Expr::Literal(Value::Null)),
            SqlLiteral::Boolean(b) => Ok(Expr::Literal(Value::Boolean(*b))),
            SqlLiteral::Integer(i) => Ok(Expr::Literal(Value::Int64(*i))),
            SqlLiteral::Float(f) => Ok(Expr::Literal(Value::Float64(*f))),
            SqlLiteral::String(s) => Ok(Expr::Literal(Value::String(s.clone()))),
        },
        SqlExpr::Identifier(col_name) => {
            if ctx.schema.get_column(col_name).is_some() {
                Ok(Expr::Column(col_name.clone()))
            } else {
                Err(Error::ColumnNotFound(col_name.clone()))
            }
        }
        SqlExpr::QualifiedIdentifier { qualifier, name } => {
            ctx.validate_qualifier(qualifier)?;
            if ctx.schema.get_column(name).is_some() {
                Ok(Expr::Column(name.clone()))
            } else {
                Err(Error::ColumnNotFound(name.clone()))
            }
        }
        SqlExpr::Unary { op, expr } => match op {
            SqlUnaryOp::Not => {
                let inner = bind_expr(expr, ctx)?;
                Ok(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(inner),
                })
            }
            SqlUnaryOp::Neg => Err(Error::SqlBindingError(
                "unary negation on non-constant expressions is not supported".to_string(),
            )),
        },
        SqlExpr::Binary { left, op, right } => {
            let mut left_bound = bind_expr(left, ctx)?;
            let mut right_bound = bind_expr(right, ctx)?;

            // Contextual literal type coercion (e.g. integer literal 42 compared to UInt64 or Float64 column)
            if let Ok(left_type) = left_bound.infer_type(ctx.schema) {
                if let Ok(right_type) = right_bound.infer_type(ctx.schema) {
                    if left_type == DataType::UInt64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            if i >= 0 {
                                right_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::UInt64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            if i >= 0 {
                                left_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        }
                    } else if left_type == DataType::Float64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            right_bound = Expr::Literal(Value::Float64(i as f64));
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::Float64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            left_bound = Expr::Literal(Value::Float64(i as f64));
                        }
                    }
                }
            }

            let bin_op = match op {
                SqlBinaryOp::Eq => BinaryOp::Eq,
                SqlBinaryOp::NotEq => BinaryOp::NotEq,
                SqlBinaryOp::Lt => BinaryOp::Lt,
                SqlBinaryOp::LtEq => BinaryOp::LtEq,
                SqlBinaryOp::Gt => BinaryOp::Gt,
                SqlBinaryOp::GtEq => BinaryOp::GtEq,
                SqlBinaryOp::And => BinaryOp::And,
                SqlBinaryOp::Or => BinaryOp::Or,
            };
            Ok(Expr::Binary {
                left: Box::new(left_bound),
                op: bin_op,
                right: Box::new(right_bound),
            })
        }

        SqlExpr::IsNull(expr) => {
            let inner = bind_expr(expr, ctx)?;
            Ok(Expr::IsNull(Box::new(inner)))
        }
        SqlExpr::IsNotNull(expr) => {
            let inner = bind_expr(expr, ctx)?;
            Ok(Expr::IsNotNull(Box::new(inner)))
        }
    }
}
