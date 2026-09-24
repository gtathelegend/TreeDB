use crate::engine::types::{DataType, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, AggregateFunction, BinaryOp, Expr, UnaryOp};
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
    /// Optional boolean filtering predicate expression (WHERE).
    pub filter: Option<Expr>,
    /// Grouping expressions (GROUP BY).
    pub group_by: Vec<Expr>,
    /// Aggregate expressions to compute.
    pub aggregates: Vec<AggregateExpr>,
    /// Optional post-aggregation boolean filtering predicate expression (HAVING).
    pub having: Option<Expr>,
    /// Ordered list of sort keys.
    pub order_by: Vec<SortKey>,
    /// Maximum rows to return (LIMIT).
    pub limit: Option<usize>,
    /// Number of rows to skip (OFFSET).
    pub offset: Option<usize>,
}

/// Helper function to check if a SQL AST expression contains any aggregate function.
pub fn contains_aggregate(expr: &SqlExpr) -> bool {
    match expr {
        SqlExpr::CountAll | SqlExpr::Aggregate { .. } => true,
        SqlExpr::Binary { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
            contains_aggregate(expr)
        }
        SqlExpr::Literal(_) | SqlExpr::Identifier(_) | SqlExpr::QualifiedIdentifier { .. } => false,
    }
}

/// Collects all unique `AggregateExpr` instances referenced within an AST expression.
fn collect_aggregates(
    expr: &SqlExpr,
    ctx: &BindingContext<'_>,
    aggs: &mut Vec<AggregateExpr>,
) -> Result<()> {
    match expr {
        SqlExpr::CountAll => {
            let agg = AggregateExpr::CountAll;
            if !aggs.contains(&agg) {
                aggs.push(agg);
            }
            Ok(())
        }
        SqlExpr::Aggregate { function, arg } => {
            if contains_aggregate(arg) {
                return Err(Error::NestedAggregate(format!(
                    "nested aggregate inside {function}: {arg}"
                )));
            }
            let bound_arg = bind_expr(arg, ctx)?;
            let func = match function {
                crate::sql::ast::AggregateFunction::Count => AggregateFunction::Count,
                crate::sql::ast::AggregateFunction::Sum => AggregateFunction::Sum,
                crate::sql::ast::AggregateFunction::Avg => AggregateFunction::Avg,
                crate::sql::ast::AggregateFunction::Min => AggregateFunction::Min,
                crate::sql::ast::AggregateFunction::Max => AggregateFunction::Max,
            };
            let agg = AggregateExpr::Function {
                func,
                expr: bound_arg,
            };
            let _ = agg.infer_type(ctx.schema)?;
            if !aggs.contains(&agg) {
                aggs.push(agg);
            }
            Ok(())
        }
        SqlExpr::Binary { left, right, .. } => {
            collect_aggregates(left, ctx, aggs)?;
            collect_aggregates(right, ctx, aggs)?;
            Ok(())
        }
        SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
            collect_aggregates(expr, ctx, aggs)
        }
        SqlExpr::Literal(_) | SqlExpr::Identifier(_) | SqlExpr::QualifiedIdentifier { .. } => {
            Ok(())
        }
    }
}

/// Binds a parsed `SelectStatement` AST against a table's `Schema`.
///
/// Performs name resolution for columns, expands wildcards (`*`, `table.*`),
/// validates qualifiers and aliases, validates grouping and aggregate semantics,
/// performs static type inference, and translates AST expressions into executable `Expr`s.
pub fn bind_select(statement: &SelectStatement, schema: &Schema) -> Result<BoundSelect> {
    let ctx = BindingContext::new(
        &statement.from.name,
        statement.from.alias.as_deref(),
        schema,
    );

    // 1. Bind WHERE filtering predicate
    let filter = if let Some(selection) = &statement.selection {
        if contains_aggregate(selection) {
            return Err(Error::AggregateInWhere);
        }
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

    // 2. Determine whether query is an aggregate / grouped query
    let has_group_by = !statement.group_by.is_empty();
    let has_having = statement.having.is_some();
    let has_proj_agg = statement.projection.iter().any(|item| match item {
        SelectItem::Expression { expr, .. } => contains_aggregate(expr),
        _ => false,
    });
    let has_order_agg = statement
        .order_by
        .iter()
        .any(|item| contains_aggregate(&item.expr));
    let is_aggregate = has_group_by || has_having || has_proj_agg || has_order_agg;

    let limit = statement.limit.map(|v| v as usize);
    let offset = statement.offset.map(|v| v as usize);

    if !is_aggregate {
        // Standard non-aggregate query path
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

        let mut order_by = Vec::with_capacity(statement.order_by.len());
        for order_item in &statement.order_by {
            // Check if sort key references a projected column alias
            let bound_expr = if let SqlExpr::Identifier(ident) = &order_item.expr {
                if let Some((proj_expr, _)) = projections
                    .iter()
                    .find(|(_, a)| a.as_deref() == Some(ident.as_str()))
                {
                    proj_expr.clone()
                } else {
                    bind_expr(&order_item.expr, &ctx)?
                }
            } else {
                bind_expr(&order_item.expr, &ctx)?
            };

            let _ = bound_expr.infer_type(schema)?;
            let direction = match order_item.direction {
                OrderDirection::Asc => SortDirection::Ascending,
                OrderDirection::Desc => SortDirection::Descending,
            };
            order_by.push(SortKey::new(bound_expr, direction));
        }

        return Ok(BoundSelect {
            table_name: statement.from.name.clone(),
            projections,
            filter,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            having: None,
            order_by,
            limit,
            offset,
        });
    }

    // 3. Aggregate / Grouped query path
    let mut bound_group_by = Vec::with_capacity(statement.group_by.len());
    for grp_expr in &statement.group_by {
        if contains_aggregate(grp_expr) {
            return Err(Error::InvalidGroupByExpression(format!(
                "aggregate functions are not allowed in GROUP BY: {grp_expr}"
            )));
        }
        let bound = bind_expr(grp_expr, &ctx)?;
        bound.infer_type(schema)?;
        bound_group_by.push(bound);
    }

    // Collect all aggregate expressions from SELECT, HAVING, and ORDER BY
    let mut aggregates = Vec::new();
    for item in &statement.projection {
        if let SelectItem::Expression { expr, .. } = item {
            collect_aggregates(expr, &ctx, &mut aggregates)?;
        }
    }
    if let Some(having) = &statement.having {
        collect_aggregates(having, &ctx, &mut aggregates)?;
    }
    for order_item in &statement.order_by {
        collect_aggregates(&order_item.expr, &ctx, &mut aggregates)?;
    }

    // Construct intermediate grouped schema for validation
    let mut grouped_cols = Vec::new();
    for (idx, grp) in bound_group_by.iter().enumerate() {
        let dt = grp.infer_type(schema)?;
        let name = match grp {
            Expr::Column(c) => c.clone(),
            _ => format!("_group_{idx}"),
        };
        grouped_cols.push(crate::engine::types::Column::new(name, dt, true));
    }
    for agg in &aggregates {
        let dt = agg.infer_type(schema)?;
        grouped_cols.push(crate::engine::types::Column::new(
            agg.display_name(),
            dt,
            true,
        ));
    }
    let grouped_schema = Schema::new_projected(grouped_cols);

    // Bind and validate SELECT projections
    let mut projections = Vec::new();
    for item in &statement.projection {
        match item {
            SelectItem::Wildcard => {
                for col in &schema.columns {
                    if !bound_group_by.contains(&Expr::Column(col.name.clone())) {
                        return Err(Error::UngroupedColumn(col.name.clone()));
                    }
                    projections.push((Expr::Column(col.name.clone()), None));
                }
            }
            SelectItem::QualifiedWildcard(qualifier) => {
                ctx.validate_qualifier(qualifier)?;
                for col in &schema.columns {
                    if !bound_group_by.contains(&Expr::Column(col.name.clone())) {
                        return Err(Error::UngroupedColumn(col.name.clone()));
                    }
                    projections.push((Expr::Column(col.name.clone()), None));
                }
            }
            SelectItem::Expression { expr, alias } => {
                let bound = bind_grouped_expr(expr, &ctx, &bound_group_by, &grouped_schema)?;
                let _ = bound.infer_type(&grouped_schema)?;
                let default_alias = match expr {
                    SqlExpr::CountAll => Some("COUNT(*)".to_string()),
                    SqlExpr::Aggregate { function, arg } => Some(format!("{function}({arg})")),
                    _ => None,
                };
                let final_alias = alias.clone().or(default_alias);
                projections.push((bound, final_alias));
            }
        }
    }

    if projections.is_empty() {
        return Err(Error::InvalidProjection(
            "projection list cannot be empty".to_string(),
        ));
    }

    // Bind and validate HAVING clause
    let having = if let Some(having_ast) = &statement.having {
        let bound_having = bind_grouped_expr(having_ast, &ctx, &bound_group_by, &grouped_schema)
            .map_err(|e| match e {
                Error::UngroupedColumn(c) => Error::InvalidHavingExpression(format!(
                    "column '{c}' must appear in GROUP BY or be aggregated"
                )),
                other => other,
            })?;
        let dt = bound_having.infer_type(&grouped_schema)?;
        if dt != DataType::Boolean && dt != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: dt.to_string(),
            });
        }
        Some(bound_having)
    } else {
        None
    };

    // Bind and validate ORDER BY clause
    let mut order_by = Vec::with_capacity(statement.order_by.len());
    for order_item in &statement.order_by {
        let bound_expr = if let SqlExpr::Identifier(ident) = &order_item.expr {
            if let Some((proj_expr, _)) = projections
                .iter()
                .find(|(_, a)| a.as_deref() == Some(ident.as_str()))
            {
                proj_expr.clone()
            } else {
                bind_grouped_expr(&order_item.expr, &ctx, &bound_group_by, &grouped_schema)?
            }
        } else {
            bind_grouped_expr(&order_item.expr, &ctx, &bound_group_by, &grouped_schema)?
        };

        let _ = bound_expr.infer_type(&grouped_schema)?;
        let direction = match order_item.direction {
            OrderDirection::Asc => SortDirection::Ascending,
            OrderDirection::Desc => SortDirection::Descending,
        };
        order_by.push(SortKey::new(bound_expr, direction));
    }

    Ok(BoundSelect {
        table_name: statement.from.name.clone(),
        projections,
        filter,
        group_by: bound_group_by,
        aggregates,
        having,
        order_by,
        limit,
        offset,
    })
}

/// Binds an expression within a grouped / aggregated context, mapping aggregate calls
/// to their computed intermediate column names and verifying that non-aggregate column
/// references belong to the `GROUP BY` clause.
pub fn bind_grouped_expr(
    expr: &SqlExpr,
    ctx: &BindingContext<'_>,
    group_by: &[Expr],
    grouped_schema: &Schema,
) -> Result<Expr> {
    match expr {
        SqlExpr::CountAll => Ok(Expr::Column("COUNT(*)".to_string())),
        SqlExpr::Aggregate { function, arg } => {
            let bound_arg = bind_expr(arg, ctx)?;
            let func = match function {
                crate::sql::ast::AggregateFunction::Count => AggregateFunction::Count,
                crate::sql::ast::AggregateFunction::Sum => AggregateFunction::Sum,
                crate::sql::ast::AggregateFunction::Avg => AggregateFunction::Avg,
                crate::sql::ast::AggregateFunction::Min => AggregateFunction::Min,
                crate::sql::ast::AggregateFunction::Max => AggregateFunction::Max,
            };
            let agg = AggregateExpr::Function {
                func,
                expr: bound_arg,
            };
            Ok(Expr::Column(agg.display_name()))
        }
        SqlExpr::Literal(lit) => match lit {
            SqlLiteral::Null => Ok(Expr::Literal(Value::Null)),
            SqlLiteral::Boolean(b) => Ok(Expr::Literal(Value::Boolean(*b))),
            SqlLiteral::Integer(i) => Ok(Expr::Literal(Value::Int64(*i))),
            SqlLiteral::Float(f) => Ok(Expr::Literal(Value::Float64(*f))),
            SqlLiteral::String(s) => Ok(Expr::Literal(Value::String(s.clone()))),
        },
        SqlExpr::Identifier(col_name) => {
            if group_by.contains(&Expr::Column(col_name.clone())) {
                Ok(Expr::Column(col_name.clone()))
            } else if ctx.schema.get_column(col_name).is_some() {
                Err(Error::UngroupedColumn(col_name.clone()))
            } else {
                Err(Error::ColumnNotFound(col_name.clone()))
            }
        }
        SqlExpr::QualifiedIdentifier { qualifier, name } => {
            ctx.validate_qualifier(qualifier)?;
            if group_by.contains(&Expr::Column(name.clone())) {
                Ok(Expr::Column(name.clone()))
            } else if ctx.schema.get_column(name).is_some() {
                Err(Error::UngroupedColumn(name.clone()))
            } else {
                Err(Error::ColumnNotFound(name.clone()))
            }
        }
        SqlExpr::Unary { op, expr } => match op {
            SqlUnaryOp::Not => {
                let inner = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
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
            let mut left_bound = bind_grouped_expr(left, ctx, group_by, grouped_schema)?;
            let mut right_bound = bind_grouped_expr(right, ctx, group_by, grouped_schema)?;

            // Contextual literal type coercion
            if let Ok(left_type) = left_bound.infer_type(grouped_schema) {
                if let Ok(right_type) = right_bound.infer_type(grouped_schema) {
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
            let inner = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
            Ok(Expr::IsNull(Box::new(inner)))
        }
        SqlExpr::IsNotNull(expr) => {
            let inner = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
            Ok(Expr::IsNotNull(Box::new(inner)))
        }
    }
}

/// Recursively binds a `SqlExpr` into a Phase 14 `Expr`.
pub fn bind_expr(expr: &SqlExpr, ctx: &BindingContext<'_>) -> Result<Expr> {
    match expr {
        SqlExpr::CountAll | SqlExpr::Aggregate { .. } => Err(Error::SqlBindingError(
            "aggregate expressions cannot be bound as scalar table expressions".to_string(),
        )),
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
