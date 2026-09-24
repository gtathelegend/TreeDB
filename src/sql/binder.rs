use crate::engine::types::{Column, DataType, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, AggregateFunction, BinaryOp, Expr, UnaryOp};
use crate::query::operator::{SortDirection, SortKey};
use crate::sql::ast::{
    JoinType, OrderDirection, SelectItem, SelectStatement, SqlBinaryOp, SqlExpr, SqlLiteral,
    SqlUnaryOp,
};

/// Represents an individual table in scope for SQL semantic binding.
#[derive(Debug, Clone)]
pub struct TableBinding<'a> {
    /// Canonical table name.
    pub name: &'a str,
    /// Optional alias specified in FROM/JOIN (`AS alias` or `alias`).
    pub alias: Option<&'a str>,
    /// Table schema.
    pub schema: &'a Schema,
}

impl<'a> TableBinding<'a> {
    /// Creates a new `TableBinding`.
    pub fn new(name: &'a str, alias: Option<&'a str>, schema: &'a Schema) -> Self {
        Self {
            name,
            alias,
            schema,
        }
    }

    /// Returns the primary qualifier for this table (alias if present, otherwise table name).
    pub fn qualifier(&self) -> &'a str {
        self.alias.unwrap_or(self.name)
    }
}

/// Semantic binding context holding the target tables in scope and combined schema information.
#[derive(Debug, Clone)]
pub struct BindingContext<'a> {
    /// All tables in FROM and JOIN clauses in order of declaration.
    pub tables: Vec<TableBinding<'a>>,
    /// Combined/intermediate schema representing all visible columns.
    pub combined_schema: Schema,
}

impl<'a> BindingContext<'a> {
    /// Creates a new `BindingContext` for a list of table bindings.
    pub fn new(tables: Vec<TableBinding<'a>>) -> Self {
        let combined_schema = if tables.len() == 1 {
            tables[0].schema.clone()
        } else {
            let mut cols = Vec::new();
            for tbl in &tables {
                let q = tbl.qualifier();
                for col in &tbl.schema.columns {
                    cols.push(Column::new(
                        format!("{q}.{}", col.name),
                        col.data_type,
                        col.nullable,
                    ));
                }
            }
            Schema::new_projected(cols)
        };
        Self {
            tables,
            combined_schema,
        }
    }

    /// Creates a single-table `BindingContext`.
    pub fn new_single(
        table_name: &'a str,
        table_alias: Option<&'a str>,
        schema: &'a Schema,
    ) -> Self {
        Self::new(vec![TableBinding::new(table_name, table_alias, schema)])
    }

    /// Primary table name (first table in FROM).
    pub fn table_name(&self) -> &str {
        self.tables.first().map(|t| t.name).unwrap_or("")
    }

    /// Primary table alias.
    pub fn table_alias(&self) -> Option<&str> {
        self.tables.first().and_then(|t| t.alias)
    }

    /// Combined schema.
    pub fn schema(&self) -> &Schema {
        &self.combined_schema
    }

    /// Resolves an unqualified column identifier across visible tables.
    pub fn resolve_column(&self, col_name: &str) -> Result<Expr> {
        let mut matches = Vec::new();
        for tbl in &self.tables {
            if tbl.schema.get_column(col_name).is_some() {
                matches.push(tbl);
            }
        }
        if matches.is_empty() {
            return Err(Error::ColumnNotFound(col_name.to_string()));
        }
        if matches.len() > 1 {
            return Err(Error::AmbiguousColumn(col_name.to_string()));
        }
        if self.tables.len() == 1 {
            Ok(Expr::Column(col_name.to_string()))
        } else {
            let q = matches[0].qualifier();
            Ok(Expr::Column(format!("{q}.{col_name}")))
        }
    }

    /// Resolves a qualified column identifier (`qualifier.col_name`).
    pub fn resolve_qualified_column(&self, qualifier: &str, col_name: &str) -> Result<Expr> {
        let mut matching_tables = Vec::new();
        for tbl in &self.tables {
            if tbl.alias == Some(qualifier) || (tbl.alias.is_none() && tbl.name == qualifier) {
                matching_tables.push(tbl);
            }
        }
        if matching_tables.is_empty() {
            return Err(Error::UnknownTableAlias(qualifier.to_string()));
        }
        if matching_tables.len() > 1 {
            return Err(Error::AmbiguousColumn(format!("{qualifier}.{col_name}")));
        }
        let tbl = matching_tables[0];
        if tbl.schema.get_column(col_name).is_none() {
            return Err(Error::ColumnNotFound(format!("{qualifier}.{col_name}")));
        }
        if self.tables.len() == 1 {
            Ok(Expr::Column(col_name.to_string()))
        } else {
            let q = tbl.qualifier();
            Ok(Expr::Column(format!("{q}.{col_name}")))
        }
    }

    /// Verifies if a given table or alias qualifier is valid in this binding context.
    pub fn validate_qualifier(&self, qualifier: &str) -> Result<&TableBinding<'a>> {
        for tbl in &self.tables {
            if tbl.alias == Some(qualifier) || (tbl.alias.is_none() && tbl.name == qualifier) {
                return Ok(tbl);
            }
        }
        Err(Error::UnknownTableAlias(qualifier.to_string()))
    }
}

/// A fully validated, bound JOIN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundJoin {
    /// Relational join strategy.
    pub join_type: JoinType,
    /// Right-hand table name being joined.
    pub table_name: String,
    /// Optional right-hand table alias.
    pub table_alias: Option<String>,
    /// Bound join condition boolean predicate.
    pub on: Expr,
}

/// A fully validated, bound SQL query ready for execution against TreeDB's query engine.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// Target base table name to query (FROM).
    pub table_name: String,
    /// Optional base table alias.
    pub table_alias: Option<String>,
    /// Bound JOIN clauses in order of evaluation.
    pub joins: Vec<BoundJoin>,
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
            let _ = agg.infer_type(&ctx.combined_schema)?;
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

/// Binds a parsed `SelectStatement` AST against a single table's `Schema`.
pub fn bind_select(statement: &SelectStatement, schema: &Schema) -> Result<BoundSelect> {
    let binding = TableBinding::new(
        &statement.from.name,
        statement.from.alias.as_deref(),
        schema,
    );
    bind_select_multi(statement, &[binding])
}

/// Binds a parsed `SelectStatement` AST against multiple tables' schemas (supporting `FROM` and `JOIN`s).
pub fn bind_select_multi<'a>(
    statement: &SelectStatement,
    tables: &[TableBinding<'a>],
) -> Result<BoundSelect> {
    let ctx = BindingContext::new(tables.to_vec());
    let schema = &ctx.combined_schema;

    // 1. Bind JOIN clauses
    let mut bound_joins = Vec::with_capacity(statement.joins.len());
    for join_clause in &statement.joins {
        if contains_aggregate(&join_clause.on) {
            return Err(Error::AggregateInWhere);
        }
        let bound_on = bind_expr(&join_clause.on, &ctx)?;
        let dt = bound_on.infer_type(schema)?;
        if dt != DataType::Boolean && dt != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: dt.to_string(),
            });
        }
        bound_joins.push(BoundJoin {
            join_type: join_clause.join_type,
            table_name: join_clause.table.name.clone(),
            table_alias: join_clause.table.alias.clone(),
            on: bound_on,
        });
    }

    // 2. Bind WHERE filtering predicate
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

    // 3. Determine whether query is an aggregate / grouped query
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
                    if tables.len() == 1 {
                        for col in &tables[0].schema.columns {
                            projections
                                .push((Expr::Column(col.name.clone()), Some(col.name.clone())));
                        }
                    } else {
                        for tbl in tables {
                            let q = tbl.qualifier();
                            for col in &tbl.schema.columns {
                                projections.push((
                                    Expr::Column(format!("{q}.{}", col.name)),
                                    Some(col.name.clone()),
                                ));
                            }
                        }
                    }
                }
                SelectItem::QualifiedWildcard(qualifier) => {
                    let tbl = ctx.validate_qualifier(qualifier)?;
                    let q = tbl.qualifier();
                    if tables.len() == 1 {
                        for col in &tbl.schema.columns {
                            projections
                                .push((Expr::Column(col.name.clone()), Some(col.name.clone())));
                        }
                    } else {
                        for col in &tbl.schema.columns {
                            projections.push((
                                Expr::Column(format!("{q}.{}", col.name)),
                                Some(col.name.clone()),
                            ));
                        }
                    }
                }
                SelectItem::Expression { expr, alias } => {
                    let bound = bind_expr(expr, &ctx)?;
                    let _ = bound.infer_type(schema)?;
                    let default_alias = match expr {
                        SqlExpr::Identifier(ident) => Some(ident.clone()),
                        SqlExpr::QualifiedIdentifier { name, .. } => Some(name.clone()),
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
            table_alias: statement.from.alias.clone(),
            joins: bound_joins,
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

    // Grouped / Aggregate query path
    let mut bound_group_by = Vec::with_capacity(statement.group_by.len());
    for gb_expr in &statement.group_by {
        if contains_aggregate(gb_expr) {
            return Err(Error::InvalidGroupByExpression(
                "GROUP BY cannot contain aggregate functions".to_string(),
            ));
        }
        let bound = bind_expr(gb_expr, &ctx)?;
        let _ = bound.infer_type(schema)?;
        bound_group_by.push(bound);
    }

    let mut aggregates = Vec::new();
    for item in &statement.projection {
        if let SelectItem::Expression { expr, .. } = item {
            collect_aggregates(expr, &ctx, &mut aggregates)?;
        }
    }
    if let Some(having_expr) = &statement.having {
        collect_aggregates(having_expr, &ctx, &mut aggregates)?;
    }
    for order_item in &statement.order_by {
        collect_aggregates(&order_item.expr, &ctx, &mut aggregates)?;
    }

    // Build intermediate schema for grouped results
    let mut grouped_columns = Vec::new();
    for expr in &bound_group_by {
        let dt = expr.infer_type(schema)?;
        let col_name = match expr {
            Expr::Column(name) => name.clone(),
            _ => format!("{expr}"),
        };
        grouped_columns.push(Column::new(col_name, dt, true));
    }
    for agg in &aggregates {
        let dt = agg.infer_type(schema)?;
        grouped_columns.push(Column::new(agg.display_name(), dt, true));
    }
    let grouped_schema = Schema::new_projected(grouped_columns);

    let mut projections = Vec::new();
    for item in &statement.projection {
        match item {
            SelectItem::Wildcard => {
                if tables.len() == 1 {
                    for col in &tables[0].schema.columns {
                        if !bound_group_by.contains(&Expr::Column(col.name.clone())) {
                            return Err(Error::UngroupedColumn(col.name.clone()));
                        }
                        projections.push((Expr::Column(col.name.clone()), Some(col.name.clone())));
                    }
                } else {
                    for tbl in tables {
                        let q = tbl.qualifier();
                        for col in &tbl.schema.columns {
                            let qual_col = format!("{q}.{}", col.name);
                            if !bound_group_by.contains(&Expr::Column(qual_col.clone())) {
                                return Err(Error::UngroupedColumn(qual_col));
                            }
                            projections.push((Expr::Column(qual_col), Some(col.name.clone())));
                        }
                    }
                }
            }
            SelectItem::QualifiedWildcard(qualifier) => {
                let tbl = ctx.validate_qualifier(qualifier)?;
                let q = tbl.qualifier();
                if tables.len() == 1 {
                    for col in &tbl.schema.columns {
                        if !bound_group_by.contains(&Expr::Column(col.name.clone())) {
                            return Err(Error::UngroupedColumn(col.name.clone()));
                        }
                        projections.push((Expr::Column(col.name.clone()), Some(col.name.clone())));
                    }
                } else {
                    for col in &tbl.schema.columns {
                        let qual_col = format!("{q}.{}", col.name);
                        if !bound_group_by.contains(&Expr::Column(qual_col.clone())) {
                            return Err(Error::UngroupedColumn(qual_col));
                        }
                        projections.push((Expr::Column(qual_col), Some(col.name.clone())));
                    }
                }
            }
            SelectItem::Expression { expr, alias } => {
                let bound = bind_grouped_expr(expr, &ctx, &bound_group_by, &grouped_schema)?;
                let _ = bound.infer_type(&grouped_schema)?;
                let default_alias = match expr {
                    SqlExpr::CountAll => Some("COUNT(*)".to_string()),
                    SqlExpr::Aggregate { function, arg } => Some(format!("{function}({arg})")),
                    SqlExpr::Identifier(ident) => Some(ident.clone()),
                    SqlExpr::QualifiedIdentifier { name, .. } => Some(name.clone()),
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
        table_alias: statement.from.alias.clone(),
        joins: bound_joins,
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
            } else {
                let resolved = ctx.resolve_column(col_name)?;
                if group_by.contains(&resolved) {
                    Ok(resolved)
                } else {
                    Err(Error::UngroupedColumn(col_name.clone()))
                }
            }
        }
        SqlExpr::QualifiedIdentifier { qualifier, name } => {
            let resolved = ctx.resolve_qualified_column(qualifier, name)?;
            if group_by.contains(&resolved) || group_by.contains(&Expr::Column(name.clone())) {
                Ok(resolved)
            } else {
                Err(Error::UngroupedColumn(format!("{qualifier}.{name}")))
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
        SqlExpr::Identifier(col_name) => ctx.resolve_column(col_name),
        SqlExpr::QualifiedIdentifier { qualifier, name } => {
            ctx.resolve_qualified_column(qualifier, name)
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
            if let Ok(left_type) = left_bound.infer_type(&ctx.combined_schema) {
                if let Ok(right_type) = right_bound.infer_type(&ctx.combined_schema) {
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
