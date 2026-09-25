use crate::engine::types::{Column, DataType, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, AggregateFunction, BinaryOp, Expr, UnaryOp};
use crate::query::operator::{SortDirection, SortKey};
use crate::sql::ast::{
    JoinType, OrderDirection, SelectItem, SelectStatement, SqlBinaryOp, SqlExpr, SqlLiteral,
    SqlUnaryOp,
};

/// Trait providing table schema and index metadata lookups during SQL binding.
pub trait TableCatalog {
    /// Retrieves the `Schema` for a table by canonical name.
    fn get_table_schema(&self, table_name: &str) -> Result<Schema>;
    /// Retrieves index metadata for a table by canonical name.
    fn get_table_indexes(
        &self,
        _table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        Ok(Vec::new())
    }
}

impl<'a> TableCatalog for &[TableBinding<'a>] {
    fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        for tbl in *self {
            if tbl.name == table_name || tbl.alias == Some(table_name) {
                return Ok(tbl.schema.clone());
            }
        }
        Err(Error::TableNotFound(table_name.to_string()))
    }
}

impl TableCatalog for std::collections::HashMap<String, Schema> {
    fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        self.get(table_name)
            .cloned()
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))
    }
}

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

/// Semantic binding context holding the target tables in scope, nested parent scopes,
/// and combined schema information.
#[derive(Clone)]
pub struct BindingContext<'a> {
    /// All tables in FROM and JOIN clauses in order of declaration.
    pub tables: Vec<TableBinding<'a>>,
    /// Combined/intermediate schema representing all visible columns in the current scope.
    pub combined_schema: Schema,
    /// Optional parent scope for correlated subqueries.
    pub parent: Option<&'a BindingContext<'a>>,
    /// Optional catalog lookup provider for finding referenced tables.
    pub catalog: Option<&'a dyn TableCatalog>,
}

impl<'a> BindingContext<'a> {
    /// Creates a new root `BindingContext` for a list of table bindings.
    pub fn new(tables: Vec<TableBinding<'a>>) -> Self {
        Self::with_parent_and_catalog(tables, None, None)
    }

    /// Creates a nested `BindingContext` with parent scope and catalog lookups.
    pub fn with_parent_and_catalog(
        tables: Vec<TableBinding<'a>>,
        parent: Option<&'a BindingContext<'a>>,
        catalog: Option<&'a dyn TableCatalog>,
    ) -> Self {
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
            parent,
            catalog,
        }
    }

    /// Creates a single-table root `BindingContext`.
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

    /// Looks up a table schema from the catalog or enclosing scopes.
    pub fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        if let Some(cat) = self.catalog {
            if let Ok(s) = cat.get_table_schema(table_name) {
                return Ok(s);
            }
        }
        for tbl in &self.tables {
            if tbl.name == table_name || tbl.alias == Some(table_name) {
                return Ok(tbl.schema.clone());
            }
        }
        if let Some(parent) = self.parent {
            return parent.get_table_schema(table_name);
        }
        Err(Error::TableNotFound(table_name.to_string()))
    }

    /// Looks up table indexes from the catalog or enclosing scopes.
    pub fn get_table_indexes(
        &self,
        table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        if let Some(cat) = self.catalog {
            if let Ok(idx) = cat.get_table_indexes(table_name) {
                return Ok(idx);
            }
        }
        if let Some(parent) = self.parent {
            return parent.get_table_indexes(table_name);
        }
        Ok(Vec::new())
    }

    /// Resolves an unqualified column identifier across visible tables and enclosing parent scopes.
    pub fn resolve_column(&self, col_name: &str) -> Result<Expr> {
        let mut matches = Vec::new();
        for tbl in &self.tables {
            if tbl.schema.get_column(col_name).is_some() {
                matches.push(tbl);
            }
        }
        if matches.len() > 1 {
            return Err(Error::AmbiguousColumn(col_name.to_string()));
        }
        if matches.len() == 1 {
            if self.tables.len() == 1 {
                return Ok(Expr::Column(col_name.to_string()));
            } else {
                let q = matches[0].qualifier();
                return Ok(Expr::Column(format!("{q}.{col_name}")));
            }
        }
        // Not found in current scope; check enclosing parent scopes
        if let Some(parent) = self.parent {
            parent.resolve_outer_column(col_name, 1)
        } else {
            Err(Error::ColumnNotFound(col_name.to_string()))
        }
    }

    /// Helper for resolving outer columns across nested parent scopes.
    pub fn resolve_outer_column(&self, col_name: &str, depth: usize) -> Result<Expr> {
        let mut matches = Vec::new();
        for tbl in &self.tables {
            if tbl.schema.get_column(col_name).is_some() {
                matches.push(tbl);
            }
        }
        if matches.len() > 1 {
            return Err(Error::AmbiguousColumn(col_name.to_string()));
        }
        if matches.len() == 1 {
            let tbl = matches[0];
            let col = tbl.schema.get_column(col_name).unwrap();
            let name = if self.tables.len() == 1 {
                col_name.to_string()
            } else {
                format!("{}.{col_name}", tbl.qualifier())
            };
            return Ok(Expr::OuterColumn {
                name,
                data_type: col.data_type,
                scope_depth: depth,
            });
        }
        if let Some(parent) = self.parent {
            parent.resolve_outer_column(col_name, depth + 1)
        } else {
            Err(Error::ColumnNotFound(col_name.to_string()))
        }
    }

    /// Resolves a qualified column identifier (`qualifier.col_name`) in current or parent scopes.
    pub fn resolve_qualified_column(&self, qualifier: &str, col_name: &str) -> Result<Expr> {
        let mut matching_tables = Vec::new();
        for tbl in &self.tables {
            if tbl.alias == Some(qualifier) || (tbl.alias.is_none() && tbl.name == qualifier) {
                matching_tables.push(tbl);
            }
        }
        if matching_tables.len() > 1 {
            return Err(Error::AmbiguousColumn(format!("{qualifier}.{col_name}")));
        }
        if matching_tables.len() == 1 {
            let tbl = matching_tables[0];
            if tbl.schema.get_column(col_name).is_none() {
                return Err(Error::ColumnNotFound(format!("{qualifier}.{col_name}")));
            }
            if self.tables.len() == 1 {
                return Ok(Expr::Column(col_name.to_string()));
            } else {
                let q = tbl.qualifier();
                return Ok(Expr::Column(format!("{q}.{col_name}")));
            }
        }
        // Not in current scope; check parent scopes
        if let Some(parent) = self.parent {
            parent.resolve_outer_qualified_column(qualifier, col_name, 1)
        } else {
            Err(Error::UnknownTableAlias(qualifier.to_string()))
        }
    }

    /// Helper for resolving qualified outer columns across nested parent scopes.
    pub fn resolve_outer_qualified_column(
        &self,
        qualifier: &str,
        col_name: &str,
        depth: usize,
    ) -> Result<Expr> {
        let mut matching_tables = Vec::new();
        for tbl in &self.tables {
            if tbl.alias == Some(qualifier) || (tbl.alias.is_none() && tbl.name == qualifier) {
                matching_tables.push(tbl);
            }
        }
        if matching_tables.len() > 1 {
            return Err(Error::AmbiguousColumn(format!("{qualifier}.{col_name}")));
        }
        if matching_tables.len() == 1 {
            let tbl = matching_tables[0];
            let col = tbl
                .schema
                .get_column(col_name)
                .ok_or_else(|| Error::ColumnNotFound(format!("{qualifier}.{col_name}")))?;
            let name = format!("{}.{col_name}", tbl.qualifier());
            return Ok(Expr::OuterColumn {
                name,
                data_type: col.data_type,
                scope_depth: depth,
            });
        }
        if let Some(parent) = self.parent {
            parent.resolve_outer_qualified_column(qualifier, col_name, depth + 1)
        } else {
            Err(Error::UnknownTableAlias(qualifier.to_string()))
        }
    }

    /// Verifies if a given table or alias qualifier is valid in this binding context.
    pub fn validate_qualifier(&self, qualifier: &str) -> Result<&TableBinding<'a>> {
        for tbl in &self.tables {
            if tbl.alias == Some(qualifier) || (tbl.alias.is_none() && tbl.name == qualifier) {
                return Ok(tbl);
            }
        }
        if let Some(parent) = self.parent {
            return parent.validate_qualifier(qualifier);
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
        SqlExpr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        SqlExpr::InSubquery { expr, .. } => contains_aggregate(expr),
        SqlExpr::Subquery(_) | SqlExpr::Exists { .. } => false,
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
        SqlExpr::InList { expr, list, .. } => {
            collect_aggregates(expr, ctx, aggs)?;
            for item in list {
                collect_aggregates(item, ctx, aggs)?;
            }
            Ok(())
        }
        SqlExpr::InSubquery { expr, .. } => collect_aggregates(expr, ctx, aggs),
        SqlExpr::Subquery(_) | SqlExpr::Exists { .. } => Ok(()),
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
    bind_select_with_context(statement, &ctx)
}

/// Binds a parsed `SelectStatement` AST against multiple tables' schemas and an optional catalog provider.
pub fn bind_select_multi_with_catalog<'a>(
    statement: &SelectStatement,
    tables: &[TableBinding<'a>],
    catalog: Option<&'a dyn TableCatalog>,
) -> Result<BoundSelect> {
    let ctx = BindingContext::with_parent_and_catalog(tables.to_vec(), None, catalog);
    bind_select_with_context(statement, &ctx)
}

/// Binds a parsed `SelectStatement` AST against a pre-configured `BindingContext`.
pub fn bind_select_with_context(
    statement: &SelectStatement,
    ctx: &BindingContext<'_>,
) -> Result<BoundSelect> {
    let tables = &ctx.tables;
    let schema = &ctx.combined_schema;

    // 1. Bind JOIN clauses
    let mut bound_joins = Vec::with_capacity(statement.joins.len());
    for join_clause in &statement.joins {
        if contains_aggregate(&join_clause.on) {
            return Err(Error::AggregateInWhere);
        }
        let bound_on = bind_expr(&join_clause.on, ctx)?;
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
        let bound = bind_expr(selection, ctx)?;
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
                    let bound = bind_expr(expr, ctx)?;
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
            let bound_expr = if let SqlExpr::Identifier(ident) = &order_item.expr {
                if let Some((proj_expr, _)) = projections
                    .iter()
                    .find(|(_, a)| a.as_deref() == Some(ident.as_str()))
                {
                    proj_expr.clone()
                } else {
                    bind_expr(&order_item.expr, ctx)?
                }
            } else {
                bind_expr(&order_item.expr, ctx)?
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
        let bound = bind_expr(gb_expr, ctx)?;
        let _ = bound.infer_type(schema)?;
        bound_group_by.push(bound);
    }

    let mut aggregates = Vec::new();
    for item in &statement.projection {
        if let SelectItem::Expression { expr, .. } = item {
            collect_aggregates(expr, ctx, &mut aggregates)?;
        }
    }
    if let Some(having_expr) = &statement.having {
        collect_aggregates(having_expr, ctx, &mut aggregates)?;
    }
    for order_item in &statement.order_by {
        collect_aggregates(&order_item.expr, ctx, &mut aggregates)?;
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
                let bound = bind_grouped_expr(expr, ctx, &bound_group_by, &grouped_schema)?;
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
        let bound_having = bind_grouped_expr(having_ast, ctx, &bound_group_by, &grouped_schema)
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
                bind_grouped_expr(&order_item.expr, ctx, &bound_group_by, &grouped_schema)?
            }
        } else {
            bind_grouped_expr(&order_item.expr, ctx, &bound_group_by, &grouped_schema)?
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

/// Binds an expression within a grouped / aggregated context.
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
        SqlExpr::InList {
            expr,
            list,
            negated,
        } => {
            let bound_expr = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
            let mut bound_list = Vec::with_capacity(list.len());
            for item in list {
                bound_list.push(bind_grouped_expr(item, ctx, group_by, grouped_schema)?);
            }
            let res = Expr::InList {
                expr: Box::new(bound_expr),
                list: bound_list,
                negated: *negated,
            };
            let _ = res.infer_type(grouped_schema)?;
            Ok(res)
        }
        SqlExpr::Subquery(subquery) => {
            bind_subquery_internal(SubqueryKindTag::Scalar, subquery, ctx)
        }
        SqlExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let bound_expr = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
            bind_subquery_internal(
                SubqueryKindTag::In {
                    expr: Box::new(bound_expr),
                    negated: *negated,
                },
                subquery,
                ctx,
            )
        }
        SqlExpr::Exists { subquery, negated } => {
            bind_subquery_internal(SubqueryKindTag::Exists { negated: *negated }, subquery, ctx)
        }
    }
}

/// Recursively binds a `SqlExpr` into an `Expr`.
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
        SqlExpr::InList {
            expr,
            list,
            negated,
        } => {
            let bound_expr = bind_expr(expr, ctx)?;
            let mut bound_list = Vec::with_capacity(list.len());
            for item in list {
                bound_list.push(bind_expr(item, ctx)?);
            }
            let res = Expr::InList {
                expr: Box::new(bound_expr),
                list: bound_list,
                negated: *negated,
            };
            let _ = res.infer_type(&ctx.combined_schema)?;
            Ok(res)
        }
        SqlExpr::Subquery(subquery) => {
            bind_subquery_internal(SubqueryKindTag::Scalar, subquery, ctx)
        }
        SqlExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let bound_expr = bind_expr(expr, ctx)?;
            bind_subquery_internal(
                SubqueryKindTag::In {
                    expr: Box::new(bound_expr),
                    negated: *negated,
                },
                subquery,
                ctx,
            )
        }
        SqlExpr::Exists { subquery, negated } => {
            bind_subquery_internal(SubqueryKindTag::Exists { negated: *negated }, subquery, ctx)
        }
    }
}

#[derive(Debug, Clone)]
enum SubqueryKindTag {
    Scalar,
    In { expr: Box<Expr>, negated: bool },
    Exists { negated: bool },
}

fn bind_subquery_internal(
    kind_tag: SubqueryKindTag,
    statement: &SelectStatement,
    outer_ctx: &BindingContext<'_>,
) -> Result<Expr> {
    // 1. Resolve table schemas for the subquery
    let mut table_schemas: Vec<(
        String,
        Option<String>,
        Schema,
        Vec<crate::engine::catalog::IndexMetadata>,
    )> = Vec::new();

    let base_schema = outer_ctx.get_table_schema(&statement.from.name)?;
    let base_indexes = outer_ctx.get_table_indexes(&statement.from.name)?;
    table_schemas.push((
        statement.from.name.clone(),
        statement.from.alias.clone(),
        base_schema,
        base_indexes,
    ));

    for join in &statement.joins {
        let schema = outer_ctx.get_table_schema(&join.table.name)?;
        let indexes = outer_ctx.get_table_indexes(&join.table.name)?;
        table_schemas.push((
            join.table.name.clone(),
            join.table.alias.clone(),
            schema,
            indexes,
        ));
    }

    let mut inner_bindings = Vec::new();
    let mut inner_opt_infos = Vec::new();
    for (name, alias, schema, indexes) in &table_schemas {
        inner_bindings.push(TableBinding::new(name, alias.as_deref(), schema));
        inner_opt_infos.push(crate::query::optimizer::TableOptInfo::new(
            name,
            alias.as_deref(),
            schema.clone(),
            indexes.clone(),
        ));
    }

    let inner_ctx =
        BindingContext::with_parent_and_catalog(inner_bindings, Some(outer_ctx), outer_ctx.catalog);

    let bound_select = bind_select_with_context(statement, &inner_ctx)?;

    // 2. Validate shape based on kind
    let is_aggregate = !bound_select.group_by.is_empty() || !bound_select.aggregates.is_empty();
    let schema_for_proj = if is_aggregate {
        let mut grouped_columns = Vec::new();
        for expr in &bound_select.group_by {
            let dt = expr.infer_type(&inner_ctx.combined_schema)?;
            let col_name = match expr {
                Expr::Column(name) => name.clone(),
                _ => format!("{expr}"),
            };
            grouped_columns.push(Column::new(col_name, dt, true));
        }
        for agg in &bound_select.aggregates {
            let dt = agg.infer_type(&inner_ctx.combined_schema)?;
            grouped_columns.push(Column::new(agg.display_name(), dt, true));
        }
        Schema::new_projected(grouped_columns)
    } else {
        inner_ctx.combined_schema.clone()
    };

    let subquery_kind = match kind_tag {
        SubqueryKindTag::Scalar => {
            if bound_select.projections.len() != 1 {
                return Err(Error::SubqueryColumnCountMismatch {
                    expected: 1,
                    found: bound_select.projections.len(),
                });
            }
            let data_type = bound_select.projections[0].0.infer_type(&schema_for_proj)?;
            crate::query::expr::SubqueryKind::Scalar { data_type }
        }
        SubqueryKindTag::In { expr, negated } => {
            if bound_select.projections.len() != 1 {
                return Err(Error::SubqueryColumnCountMismatch {
                    expected: 1,
                    found: bound_select.projections.len(),
                });
            }
            let left_type = expr.infer_type(&outer_ctx.combined_schema)?;
            let right_type = bound_select.projections[0].0.infer_type(&schema_for_proj)?;
            if left_type != DataType::Null
                && right_type != DataType::Null
                && left_type != right_type
            {
                return Err(Error::InvalidComparison {
                    left: left_type.to_string(),
                    right: right_type.to_string(),
                    op: "IN".to_string(),
                });
            }
            crate::query::expr::SubqueryKind::In { expr, negated }
        }
        SubqueryKindTag::Exists { negated } => crate::query::expr::SubqueryKind::Exists { negated },
    };

    // 3. Optimize the subquery plan
    let inner_plan = crate::query::optimizer::Optimizer::optimize(&bound_select, &inner_opt_infos)?;

    // 4. Collect outer references
    let mut outer_references = Vec::new();
    collect_outer_refs_from_select(&bound_select, &mut outer_references);
    let is_correlated = !outer_references.is_empty();

    Ok(Expr::Subquery(Box::new(crate::query::expr::SubqueryExpr {
        kind: subquery_kind,
        statement: Box::new(statement.clone()),
        plan: inner_plan,
        is_correlated,
        outer_references,
        materialized: None,
    })))
}

fn collect_outer_refs_from_select(select: &BoundSelect, refs: &mut Vec<String>) {
    for (proj, _) in &select.projections {
        collect_outer_refs_from_expr(proj, refs);
    }
    if let Some(filter) = &select.filter {
        collect_outer_refs_from_expr(filter, refs);
    }
    for join in &select.joins {
        collect_outer_refs_from_expr(&join.on, refs);
    }
    for gb in &select.group_by {
        collect_outer_refs_from_expr(gb, refs);
    }
    for agg in &select.aggregates {
        if let AggregateExpr::Function { expr, .. } = agg {
            collect_outer_refs_from_expr(expr, refs);
        }
    }
    if let Some(having) = &select.having {
        collect_outer_refs_from_expr(having, refs);
    }
    for sort_key in &select.order_by {
        collect_outer_refs_from_expr(&sort_key.expression, refs);
    }
}

fn collect_outer_refs_from_expr(expr: &Expr, refs: &mut Vec<String>) {
    match expr {
        Expr::OuterColumn { name, .. } => {
            if !refs.contains(name) {
                refs.push(name.clone());
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_outer_refs_from_expr(left, refs);
            collect_outer_refs_from_expr(right, refs);
        }
        Expr::Unary { expr, .. } | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            collect_outer_refs_from_expr(expr, refs);
        }
        Expr::InList { expr, list, .. } => {
            collect_outer_refs_from_expr(expr, refs);
            for item in list {
                collect_outer_refs_from_expr(item, refs);
            }
        }
        Expr::Subquery(sub) => {
            for r in &sub.outer_references {
                if !refs.contains(r) {
                    refs.push(r.clone());
                }
            }
        }
        Expr::Literal(_) | Expr::Column(_) => {}
    }
}
