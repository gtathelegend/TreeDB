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
    /// Retrieves index metadata for an index by canonical index name.
    fn get_index(
        &self,
        _index_name: &str,
    ) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        Ok(None)
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
    /// Optional parameter values for prepared statement execution.
    pub params: Option<&'a [Value]>,
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
        let params = parent.and_then(|p| p.params);
        Self {
            tables,
            combined_schema,
            parent,
            catalog,
            params,
        }
    }

    /// Attaches parameter values to this binding context.
    pub fn with_params(mut self, params: Option<&'a [Value]>) -> Self {
        self.params = params;
        self
    }

    /// Retrieves a parameter value by 1-indexed identifier.
    pub fn get_param(&self, index: usize) -> Result<&Value> {
        if let Some(params) = self.params {
            if index == 0 || index > params.len() {
                return Err(Error::ParameterCountMismatch {
                    expected: index,
                    received: params.len(),
                });
            }
            return Ok(&params[index - 1]);
        }
        if let Some(parent) = self.parent {
            return parent.get_param(index);
        }
        Err(Error::ParameterRequired(index))
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

/// Kind of execution plan underlying a Common Table Expression.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum BoundCteKind {
    /// Standard non-recursive CTE evaluated once.
    NonRecursive {
        /// Fully optimized physical execution plan.
        plan: crate::query::optimizer::PhysicalPlan,
    },
    /// Recursive CTE evaluated iteratively until fixpoint or configured limits.
    Recursive {
        /// Anchor query physical plan.
        anchor_plan: crate::query::optimizer::PhysicalPlan,
        /// Recursive member query physical plan.
        recursive_plan: crate::query::optimizer::PhysicalPlan,
        /// Maximum allowed recursive iterations.
        max_iterations: usize,
        /// Maximum allowed accumulated generated rows.
        max_rows: usize,
    },
}

/// A bound Common Table Expression (CTE) definition.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundCte {
    /// Name of the CTE relation.
    pub name: String,
    /// Output schema produced by the CTE query projection.
    pub output_schema: Schema,
    /// Execution kind and physical plans.
    pub kind: BoundCteKind,
    /// Names of any earlier CTE relations that this CTE depends on.
    pub dependencies: Vec<String>,
}

impl BoundCte {
    /// Returns true if this CTE is recursive.
    pub fn is_recursive(&self) -> bool {
        matches!(self.kind, BoundCteKind::Recursive { .. })
    }
}

/// A fully validated, bound SQL query ready for execution against TreeDB's query engine.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// Statement-local CTE definitions bound in left-to-right dependency order.
    pub ctes: Vec<BoundCte>,
    /// Target base table name to query (FROM).
    pub table_name: String,
    /// Optional base table alias.
    pub table_alias: Option<String>,
    /// Bound JOIN clauses in order of evaluation.
    pub joins: Vec<BoundJoin>,
    /// Window expressions to compute before projection.
    pub window_exprs: Vec<crate::query::expr::WindowExpr>,
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

/// Fully validated, bound SQL `CREATE INDEX` statement ready for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCreateIndex {
    /// Name of the index.
    pub name: String,
    /// Target table name.
    pub table_name: String,
    /// Target column name.
    pub column_name: String,
    /// Target column index in the table schema.
    pub column_index: usize,
    /// Target column data type.
    pub data_type: DataType,
    /// Whether this index enforces uniqueness.
    pub unique: bool,
}

/// Fully validated, bound SQL `DROP INDEX` statement ready for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundDropIndex {
    /// Name of the index to drop.
    pub name: String,
}

/// Validates and binds a `CREATE INDEX` AST statement against the catalog and table schema.
pub fn bind_create_index(
    stmt: &crate::sql::CreateIndexStatement,
    catalog: &dyn TableCatalog,
) -> Result<BoundCreateIndex> {
    crate::engine::catalog::validate_index_name(&stmt.name)?;

    // 1. Resolve table schema through catalog
    let schema = catalog.get_table_schema(&stmt.table_name)?;

    // 2. Resolve column in table schema
    let (col_idx, col) = schema
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == stmt.column_name)
        .ok_or_else(|| Error::IndexColumnNotFound(stmt.column_name.clone()))?;

    // 3. Check duplicate index name
    if catalog.get_index(&stmt.name)?.is_some() {
        return Err(Error::DuplicateIndexName(stmt.name.clone()));
    }

    // 4. Validate data type is supported by IndexKey
    match col.data_type {
        DataType::Boolean
        | DataType::Int64
        | DataType::UInt64
        | DataType::Float64
        | DataType::String
        | DataType::Bytes => {}
        DataType::Null => {
            return Err(Error::InvalidIndexDefinition(
                "cannot index column of type Null".to_string(),
            ));
        }
    }

    Ok(BoundCreateIndex {
        name: stmt.name.clone(),
        table_name: stmt.table_name.clone(),
        column_name: stmt.column_name.clone(),
        column_index: col_idx,
        data_type: col.data_type,
        unique: stmt.unique,
    })
}

/// Validates and binds a `DROP INDEX` AST statement against the catalog.
pub fn bind_drop_index(
    stmt: &crate::sql::DropIndexStatement,
    catalog: &dyn TableCatalog,
) -> Result<BoundDropIndex> {
    crate::engine::catalog::validate_index_name(&stmt.name)?;

    if catalog.get_index(&stmt.name)?.is_none() {
        return Err(Error::IndexNotFound(stmt.name.clone()));
    }

    Ok(BoundDropIndex {
        name: stmt.name.clone(),
    })
}

/// Helper function to check if a SQL AST expression contains any window function.
pub fn contains_window(expr: &SqlExpr) -> bool {
    match expr {
        SqlExpr::Window { .. } => true,
        SqlExpr::Binary { left, right, .. } => contains_window(left) || contains_window(right),
        SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
            contains_window(expr)
        }
        SqlExpr::InList { expr, list, .. } => {
            contains_window(expr) || list.iter().any(contains_window)
        }
        SqlExpr::InSubquery { expr, .. } | SqlExpr::InSetSubquery { expr, .. } => {
            contains_window(expr)
        }
        SqlExpr::Aggregate { arg, .. } => contains_window(arg),
        SqlExpr::Subquery(_)
        | SqlExpr::SetSubquery(_)
        | SqlExpr::Exists { .. }
        | SqlExpr::SetExists { .. } => false,
        SqlExpr::CountAll
        | SqlExpr::Literal(_)
        | SqlExpr::Parameter(_)
        | SqlExpr::Identifier(_)
        | SqlExpr::QualifiedIdentifier { .. } => false,
    }
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
        SqlExpr::InSubquery { expr, .. } | SqlExpr::InSetSubquery { expr, .. } => {
            contains_aggregate(expr)
        }
        SqlExpr::Window {
            args, window_spec, ..
        } => {
            args.iter().any(contains_aggregate)
                || window_spec.partition_by.iter().any(contains_aggregate)
                || window_spec
                    .order_by
                    .iter()
                    .any(|o| contains_aggregate(&o.expr))
        }
        SqlExpr::Subquery(_)
        | SqlExpr::SetSubquery(_)
        | SqlExpr::Exists { .. }
        | SqlExpr::SetExists { .. } => false,
        SqlExpr::Literal(_)
        | SqlExpr::Parameter(_)
        | SqlExpr::Identifier(_)
        | SqlExpr::QualifiedIdentifier { .. } => false,
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
        SqlExpr::Window {
            args, window_spec, ..
        } => {
            for arg in args {
                collect_aggregates(arg, ctx, aggs)?;
            }
            for p in &window_spec.partition_by {
                collect_aggregates(p, ctx, aggs)?;
            }
            for o in &window_spec.order_by {
                collect_aggregates(&o.expr, ctx, aggs)?;
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
        SqlExpr::InSubquery { expr, .. } | SqlExpr::InSetSubquery { expr, .. } => {
            collect_aggregates(expr, ctx, aggs)
        }
        SqlExpr::Subquery(_)
        | SqlExpr::SetSubquery(_)
        | SqlExpr::Exists { .. }
        | SqlExpr::SetExists { .. } => Ok(()),
        SqlExpr::Literal(_)
        | SqlExpr::Identifier(_)
        | SqlExpr::QualifiedIdentifier { .. }
        | SqlExpr::Parameter(_) => Ok(()),
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
    bind_select_multi_with_catalog_and_params(statement, tables, catalog, None)
}

/// Binds a parsed `SelectStatement` AST against multiple tables' schemas, optional catalog, and positional parameters.
pub fn bind_select_multi_with_catalog_and_params<'a>(
    statement: &SelectStatement,
    tables: &[TableBinding<'a>],
    catalog: Option<&'a dyn TableCatalog>,
    params: Option<&'a [Value]>,
) -> Result<BoundSelect> {
    let mut ctx = BindingContext::with_parent_and_catalog(tables.to_vec(), None, catalog);
    ctx.params = params;
    bind_select_with_context(statement, &ctx)
}

/// Binds a parsed `SelectStatement` AST against a pre-configured `BindingContext`.
pub fn bind_select_with_context(
    statement: &SelectStatement,
    ctx: &BindingContext<'_>,
) -> Result<BoundSelect> {
    let tables = &ctx.tables;
    let schema = &ctx.combined_schema;

    // Validate window placement
    if let Some(selection) = &statement.selection {
        if contains_window(selection) {
            return Err(Error::InvalidWindowPlacement("WHERE clause"));
        }
    }
    for join_clause in &statement.joins {
        if contains_window(&join_clause.on) {
            return Err(Error::InvalidWindowPlacement("JOIN ON clause"));
        }
    }
    for gb_expr in &statement.group_by {
        if contains_window(gb_expr) {
            return Err(Error::InvalidWindowPlacement("GROUP BY clause"));
        }
    }
    if let Some(having_expr) = &statement.having {
        if contains_window(having_expr) {
            return Err(Error::InvalidWindowPlacement("HAVING clause"));
        }
    }

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
        let mut window_exprs = Vec::new();
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
                    let (bound, final_alias) = if let SqlExpr::Window {
                        function,
                        args,
                        window_spec,
                    } = expr
                    {
                        let we = bind_window_expr(*function, args, window_spec, ctx, schema)?;
                        let col_name = we.display_name.clone();
                        let default_alias = alias.clone().or_else(|| Some(col_name.clone()));
                        window_exprs.push(we);
                        (Expr::Column(col_name), default_alias)
                    } else {
                        let bound = bind_expr(expr, ctx)?;
                        let _ = bound.infer_type(schema)?;
                        let default_alias = match expr {
                            SqlExpr::Identifier(ident) => Some(ident.clone()),
                            SqlExpr::QualifiedIdentifier { name, .. } => Some(name.clone()),
                            _ => None,
                        };
                        let final_alias = alias.clone().or(default_alias);
                        (bound, final_alias)
                    };
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
            } else if let SqlExpr::Window {
                function,
                args,
                window_spec,
            } = &order_item.expr
            {
                let we = bind_window_expr(*function, args, window_spec, ctx, schema)?;
                let col_name = we.display_name.clone();
                if !window_exprs
                    .iter()
                    .any(|existing: &crate::query::expr::WindowExpr| {
                        existing.display_name == col_name
                    })
                {
                    window_exprs.push(we);
                }
                Expr::Column(col_name)
            } else {
                bind_expr(&order_item.expr, ctx)?
            };

            let _ = if let Expr::Column(cname) = &bound_expr {
                if let Some(we) = window_exprs.iter().find(|w| &w.display_name == cname) {
                    we.output_type
                } else {
                    bound_expr.infer_type(schema)?
                }
            } else {
                bound_expr.infer_type(schema)?
            };

            let direction = match order_item.direction {
                OrderDirection::Asc => SortDirection::Ascending,
                OrderDirection::Desc => SortDirection::Descending,
            };
            order_by.push(SortKey::new(bound_expr, direction));
        }

        return Ok(BoundSelect {
            ctes: Vec::new(),
            table_name: statement.from.name.clone(),
            table_alias: statement.from.alias.clone(),
            joins: bound_joins,
            window_exprs,
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

    let mut window_exprs = Vec::new();
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
                let (bound, final_alias) = if let SqlExpr::Window {
                    function,
                    args,
                    window_spec,
                } = expr
                {
                    let we = bind_grouped_window_expr(
                        *function,
                        args,
                        window_spec,
                        ctx,
                        &bound_group_by,
                        &grouped_schema,
                    )?;
                    let col_name = we.display_name.clone();
                    let default_alias = alias.clone().or_else(|| Some(col_name.clone()));
                    window_exprs.push(we);
                    (Expr::Column(col_name), default_alias)
                } else {
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
                    (bound, final_alias)
                };
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
        } else if let SqlExpr::Window {
            function,
            args,
            window_spec,
        } = &order_item.expr
        {
            let we = bind_grouped_window_expr(
                *function,
                args,
                window_spec,
                ctx,
                &bound_group_by,
                &grouped_schema,
            )?;
            let col_name = we.display_name.clone();
            if !window_exprs
                .iter()
                .any(|existing: &crate::query::expr::WindowExpr| existing.display_name == col_name)
            {
                window_exprs.push(we);
            }
            Expr::Column(col_name)
        } else {
            bind_grouped_expr(&order_item.expr, ctx, &bound_group_by, &grouped_schema)?
        };

        let _ = if let Expr::Column(cname) = &bound_expr {
            if let Some(we) = window_exprs.iter().find(|w| &w.display_name == cname) {
                we.output_type
            } else {
                bound_expr.infer_type(&grouped_schema)?
            }
        } else {
            bound_expr.infer_type(&grouped_schema)?
        };

        let direction = match order_item.direction {
            OrderDirection::Asc => SortDirection::Ascending,
            OrderDirection::Desc => SortDirection::Descending,
        };
        order_by.push(SortKey::new(bound_expr, direction));
    }

    Ok(BoundSelect {
        ctes: Vec::new(),
        table_name: statement.from.name.clone(),
        table_alias: statement.from.alias.clone(),
        joins: bound_joins,
        window_exprs,
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

fn validate_frame(frame: &crate::sql::ast::WindowFrame) -> Result<()> {
    match (&frame.start, &frame.end) {
        (crate::sql::ast::WindowFrameBound::UnboundedFollowing, _) => {
            return Err(Error::InvalidWindowFrame(
                "frame start cannot be UNBOUNDED FOLLOWING".to_string(),
            ));
        }
        (_, crate::sql::ast::WindowFrameBound::UnboundedPreceding) => {
            return Err(Error::InvalidWindowFrame(
                "frame end cannot be UNBOUNDED PRECEDING".to_string(),
            ));
        }
        (
            crate::sql::ast::WindowFrameBound::CurrentRow,
            crate::sql::ast::WindowFrameBound::Preceding(_),
        ) => {
            return Err(Error::InvalidWindowFrame(
                "frame start CURRENT ROW cannot precede PRECEDING end bound".to_string(),
            ));
        }
        (
            crate::sql::ast::WindowFrameBound::Following(_),
            crate::sql::ast::WindowFrameBound::Preceding(_),
        ) => {
            return Err(Error::InvalidWindowFrame(
                "frame start FOLLOWING cannot precede PRECEDING end bound".to_string(),
            ));
        }
        (
            crate::sql::ast::WindowFrameBound::Following(_),
            crate::sql::ast::WindowFrameBound::CurrentRow,
        ) => {
            return Err(Error::InvalidWindowFrame(
                "frame start FOLLOWING cannot precede CURRENT ROW end bound".to_string(),
            ));
        }
        (
            crate::sql::ast::WindowFrameBound::Following(start_n),
            crate::sql::ast::WindowFrameBound::Following(end_n),
        ) => {
            if start_n > end_n {
                return Err(Error::InvalidWindowFrame(format!(
                    "frame start {start_n} FOLLOWING is greater than end {end_n} FOLLOWING"
                )));
            }
        }
        (
            crate::sql::ast::WindowFrameBound::Preceding(start_n),
            crate::sql::ast::WindowFrameBound::Preceding(end_n),
        ) if start_n < end_n => {
            return Err(Error::InvalidWindowFrame(format!(
                "frame start {start_n} PRECEDING is less than end {end_n} PRECEDING"
            )));
        }
        _ => {}
    }
    Ok(())
}

fn bind_window_expr(
    function: crate::sql::ast::WindowFunction,
    args: &[SqlExpr],
    window_spec: &crate::sql::ast::WindowSpec,
    ctx: &BindingContext<'_>,
    schema: &Schema,
) -> Result<crate::query::expr::WindowExpr> {
    // 1. Nested window check
    for arg in args {
        if contains_window(arg) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside {function}: {arg}"
            )));
        }
    }
    for p in &window_spec.partition_by {
        if contains_window(p) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside PARTITION BY: {p}"
            )));
        }
    }
    for o in &window_spec.order_by {
        if contains_window(&o.expr) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside ORDER BY: {}",
                o.expr
            )));
        }
    }

    // 2. Validate function argument count and semantics
    let mut bound_args = Vec::with_capacity(args.len());
    let mut offset = None;
    let mut default = None;
    let output_type;

    match function {
        crate::sql::ast::WindowFunction::RowNumber
        | crate::sql::ast::WindowFunction::Rank
        | crate::sql::ast::WindowFunction::DenseRank => {
            if !args.is_empty() {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes 0 arguments, found {}",
                    args.len()
                )));
            }
            output_type = DataType::UInt64;
        }
        crate::sql::ast::WindowFunction::FirstValue
        | crate::sql::ast::WindowFunction::LastValue => {
            if args.len() != 1 {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes exactly 1 argument, found {}",
                    args.len()
                )));
            }
            let b_arg = bind_expr(&args[0], ctx)?;
            output_type = b_arg.infer_type(schema)?;
            bound_args.push(b_arg);
        }
        crate::sql::ast::WindowFunction::Lag | crate::sql::ast::WindowFunction::Lead => {
            if args.is_empty() || args.len() > 3 {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes 1 to 3 arguments, found {}",
                    args.len()
                )));
            }
            let b_arg = bind_expr(&args[0], ctx)?;
            output_type = b_arg.infer_type(schema)?;
            bound_args.push(b_arg);

            if args.len() >= 2 {
                match &args[1] {
                    SqlExpr::Literal(SqlLiteral::Integer(i)) => {
                        if *i < 0 {
                            return Err(Error::InvalidWindowOffset(format!(
                                "{function} offset cannot be negative: {i}"
                            )));
                        }
                        offset = Some(*i as usize);
                    }
                    SqlExpr::Literal(SqlLiteral::Null) => {
                        return Err(Error::InvalidWindowOffset(format!(
                            "{function} offset cannot be NULL"
                        )));
                    }
                    other => {
                        return Err(Error::InvalidWindowOffset(format!(
                            "{function} offset must be a non-negative integer constant, found: {other}"
                        )));
                    }
                }
            } else {
                offset = Some(1);
            }

            if args.len() == 3 {
                let mut def = bind_expr(&args[2], ctx)?;
                if let Ok(def_type) = def.infer_type(schema) {
                    if def_type != DataType::Null && def_type != output_type {
                        if output_type == DataType::UInt64 && def_type == DataType::Int64 {
                            if let Expr::Literal(Value::Int64(i)) = def {
                                if i >= 0 {
                                    def = Expr::Literal(Value::UInt64(i as u64));
                                }
                            }
                        } else if output_type == DataType::Float64 && def_type == DataType::Int64 {
                            if let Expr::Literal(Value::Int64(i)) = def {
                                def = Expr::Literal(Value::Float64(i as f64));
                            }
                        }
                    }
                }
                default = Some(def);
            }
        }
    }

    // 3. Bind Partition By
    let mut bound_partition_by = Vec::with_capacity(window_spec.partition_by.len());
    for p in &window_spec.partition_by {
        let b = bind_expr(p, ctx)?;
        let _ = b.infer_type(schema)?;
        bound_partition_by.push(b);
    }

    // 4. Bind Order By
    let mut bound_order_by = Vec::with_capacity(window_spec.order_by.len());
    for o in &window_spec.order_by {
        let b = bind_expr(&o.expr, ctx)?;
        let _ = b.infer_type(schema)?;
        let direction = match o.direction {
            OrderDirection::Asc => SortDirection::Ascending,
            OrderDirection::Desc => SortDirection::Descending,
        };
        bound_order_by.push(SortKey::new(b, direction));
    }

    // 5. Frame calculation and validation
    let bound_frame = if let Some(frame) = &window_spec.frame {
        validate_frame(frame)?;
        crate::query::expr::BoundWindowFrame {
            start: frame.start.clone(),
            end: frame.end.clone(),
        }
    } else if !bound_order_by.is_empty() {
        crate::query::expr::BoundWindowFrame {
            start: crate::sql::ast::WindowFrameBound::UnboundedPreceding,
            end: crate::sql::ast::WindowFrameBound::CurrentRow,
        }
    } else {
        crate::query::expr::BoundWindowFrame {
            start: crate::sql::ast::WindowFrameBound::UnboundedPreceding,
            end: crate::sql::ast::WindowFrameBound::UnboundedFollowing,
        }
    };

    let bound_spec = crate::query::expr::BoundWindowSpec {
        partition_by: bound_partition_by,
        order_by: bound_order_by,
        frame: bound_frame,
    };

    let display_name = {
        let mut arg_strs: Vec<String> = bound_args.iter().map(|e| e.to_string()).collect();
        if let Some(off) = offset {
            if off != 1 || default.is_some() {
                arg_strs.push(off.to_string());
            }
        }
        if let Some(def) = &default {
            arg_strs.push(def.to_string());
        }
        format!("{function}({}) {bound_spec}", arg_strs.join(", "))
    };

    Ok(crate::query::expr::WindowExpr {
        function,
        args: bound_args,
        offset,
        default,
        window_spec: bound_spec,
        output_type,
        display_name,
    })
}

fn bind_grouped_window_expr(
    function: crate::sql::ast::WindowFunction,
    args: &[SqlExpr],
    window_spec: &crate::sql::ast::WindowSpec,
    ctx: &BindingContext<'_>,
    group_by: &[Expr],
    grouped_schema: &Schema,
) -> Result<crate::query::expr::WindowExpr> {
    // 1. Nested window check
    for arg in args {
        if contains_window(arg) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside {function}: {arg}"
            )));
        }
    }
    for p in &window_spec.partition_by {
        if contains_window(p) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside PARTITION BY: {p}"
            )));
        }
    }
    for o in &window_spec.order_by {
        if contains_window(&o.expr) {
            return Err(Error::NestedWindowFunction(format!(
                "nested window function inside ORDER BY: {}",
                o.expr
            )));
        }
    }

    // 2. Validate function argument count and semantics
    let mut bound_args = Vec::with_capacity(args.len());
    let mut offset = None;
    let mut default = None;
    let output_type;

    match function {
        crate::sql::ast::WindowFunction::RowNumber
        | crate::sql::ast::WindowFunction::Rank
        | crate::sql::ast::WindowFunction::DenseRank => {
            if !args.is_empty() {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes 0 arguments, found {}",
                    args.len()
                )));
            }
            output_type = DataType::UInt64;
        }
        crate::sql::ast::WindowFunction::FirstValue
        | crate::sql::ast::WindowFunction::LastValue => {
            if args.len() != 1 {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes exactly 1 argument, found {}",
                    args.len()
                )));
            }
            let b_arg = bind_grouped_expr(&args[0], ctx, group_by, grouped_schema)?;
            output_type = b_arg.infer_type(grouped_schema)?;
            bound_args.push(b_arg);
        }
        crate::sql::ast::WindowFunction::Lag | crate::sql::ast::WindowFunction::Lead => {
            if args.is_empty() || args.len() > 3 {
                return Err(Error::SqlBindingError(format!(
                    "{function}() takes 1 to 3 arguments, found {}",
                    args.len()
                )));
            }
            let b_arg = bind_grouped_expr(&args[0], ctx, group_by, grouped_schema)?;
            output_type = b_arg.infer_type(grouped_schema)?;
            bound_args.push(b_arg);

            if args.len() >= 2 {
                match &args[1] {
                    SqlExpr::Literal(SqlLiteral::Integer(i)) => {
                        if *i < 0 {
                            return Err(Error::InvalidWindowOffset(format!(
                                "{function} offset cannot be negative: {i}"
                            )));
                        }
                        offset = Some(*i as usize);
                    }
                    SqlExpr::Literal(SqlLiteral::Null) => {
                        return Err(Error::InvalidWindowOffset(format!(
                            "{function} offset cannot be NULL"
                        )));
                    }
                    other => {
                        return Err(Error::InvalidWindowOffset(format!(
                            "{function} offset must be a non-negative integer constant, found: {other}"
                        )));
                    }
                }
            } else {
                offset = Some(1);
            }

            if args.len() == 3 {
                let mut def = bind_grouped_expr(&args[2], ctx, group_by, grouped_schema)?;
                if let Ok(def_type) = def.infer_type(grouped_schema) {
                    if def_type != DataType::Null && def_type != output_type {
                        if output_type == DataType::UInt64 && def_type == DataType::Int64 {
                            if let Expr::Literal(Value::Int64(i)) = def {
                                if i >= 0 {
                                    def = Expr::Literal(Value::UInt64(i as u64));
                                }
                            }
                        } else if output_type == DataType::Float64 && def_type == DataType::Int64 {
                            if let Expr::Literal(Value::Int64(i)) = def {
                                def = Expr::Literal(Value::Float64(i as f64));
                            }
                        }
                    }
                }
                default = Some(def);
            }
        }
    }

    // 3. Bind Partition By
    let mut bound_partition_by = Vec::with_capacity(window_spec.partition_by.len());
    for p in &window_spec.partition_by {
        let b = bind_grouped_expr(p, ctx, group_by, grouped_schema)?;
        let _ = b.infer_type(grouped_schema)?;
        bound_partition_by.push(b);
    }

    // 4. Bind Order By
    let mut bound_order_by = Vec::with_capacity(window_spec.order_by.len());
    for o in &window_spec.order_by {
        let b = bind_grouped_expr(&o.expr, ctx, group_by, grouped_schema)?;
        let _ = b.infer_type(grouped_schema)?;
        let direction = match o.direction {
            OrderDirection::Asc => SortDirection::Ascending,
            OrderDirection::Desc => SortDirection::Descending,
        };
        bound_order_by.push(SortKey::new(b, direction));
    }

    // 5. Frame
    let bound_frame = if let Some(frame) = &window_spec.frame {
        validate_frame(frame)?;
        crate::query::expr::BoundWindowFrame {
            start: frame.start.clone(),
            end: frame.end.clone(),
        }
    } else if !bound_order_by.is_empty() {
        crate::query::expr::BoundWindowFrame {
            start: crate::sql::ast::WindowFrameBound::UnboundedPreceding,
            end: crate::sql::ast::WindowFrameBound::CurrentRow,
        }
    } else {
        crate::query::expr::BoundWindowFrame {
            start: crate::sql::ast::WindowFrameBound::UnboundedPreceding,
            end: crate::sql::ast::WindowFrameBound::UnboundedFollowing,
        }
    };

    let bound_spec = crate::query::expr::BoundWindowSpec {
        partition_by: bound_partition_by,
        order_by: bound_order_by,
        frame: bound_frame,
    };

    let display_name = {
        let mut arg_strs: Vec<String> = bound_args.iter().map(|e| e.to_string()).collect();
        if let Some(off) = offset {
            if off != 1 || default.is_some() {
                arg_strs.push(off.to_string());
            }
        }
        if let Some(def) = &default {
            arg_strs.push(def.to_string());
        }
        format!("{function}({}) {bound_spec}", arg_strs.join(", "))
    };

    Ok(crate::query::expr::WindowExpr {
        function,
        args: bound_args,
        offset,
        default,
        window_spec: bound_spec,
        output_type,
        display_name,
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
        SqlExpr::Parameter(p_idx) => {
            let val = ctx.get_param(*p_idx)?;
            Ok(Expr::Parameter {
                index: *p_idx,
                value: val.clone(),
            })
        }
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

            // Contextual literal/parameter type coercion
            if let Ok(left_type) = left_bound.infer_type(grouped_schema) {
                if let Ok(right_type) = right_bound.infer_type(grouped_schema) {
                    if left_type == DataType::UInt64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            if i >= 0 {
                                right_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = right_bound
                        {
                            if i >= 0 {
                                right_bound = Expr::Parameter {
                                    index,
                                    value: Value::UInt64(i as u64),
                                };
                            }
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::UInt64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            if i >= 0 {
                                left_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = left_bound
                        {
                            if i >= 0 {
                                left_bound = Expr::Parameter {
                                    index,
                                    value: Value::UInt64(i as u64),
                                };
                            }
                        }
                    } else if left_type == DataType::Float64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            right_bound = Expr::Literal(Value::Float64(i as f64));
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = right_bound
                        {
                            right_bound = Expr::Parameter {
                                index,
                                value: Value::Float64(i as f64),
                            };
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::Float64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            left_bound = Expr::Literal(Value::Float64(i as f64));
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = left_bound
                        {
                            left_bound = Expr::Parameter {
                                index,
                                value: Value::Float64(i as f64),
                            };
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
                SqlBinaryOp::Add => BinaryOp::Add,
                SqlBinaryOp::Sub => BinaryOp::Sub,
                SqlBinaryOp::Mul => BinaryOp::Mul,
                SqlBinaryOp::Div => BinaryOp::Div,
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
        SqlExpr::SetSubquery(subquery) => {
            bind_set_subquery_internal(SubqueryKindTag::Scalar, subquery, ctx)
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
        SqlExpr::InSetSubquery {
            expr,
            subquery,
            negated,
        } => {
            let bound_expr = bind_grouped_expr(expr, ctx, group_by, grouped_schema)?;
            bind_set_subquery_internal(
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
        SqlExpr::SetExists { subquery, negated } => {
            bind_set_subquery_internal(SubqueryKindTag::Exists { negated: *negated }, subquery, ctx)
        }
        SqlExpr::Window { .. } => Err(Error::SqlBindingError(
            "window expressions cannot be bound as scalar expressions directly".to_string(),
        )),
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
        SqlExpr::Parameter(p_idx) => {
            let val = ctx.get_param(*p_idx)?;
            Ok(Expr::Parameter {
                index: *p_idx,
                value: val.clone(),
            })
        }
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

            // Contextual literal/parameter type coercion (e.g. integer literal/parameter 42 compared to UInt64 or Float64 column)
            if let Ok(left_type) = left_bound.infer_type(&ctx.combined_schema) {
                if let Ok(right_type) = right_bound.infer_type(&ctx.combined_schema) {
                    if left_type == DataType::UInt64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            if i >= 0 {
                                right_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = right_bound
                        {
                            if i >= 0 {
                                right_bound = Expr::Parameter {
                                    index,
                                    value: Value::UInt64(i as u64),
                                };
                            }
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::UInt64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            if i >= 0 {
                                left_bound = Expr::Literal(Value::UInt64(i as u64));
                            }
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = left_bound
                        {
                            if i >= 0 {
                                left_bound = Expr::Parameter {
                                    index,
                                    value: Value::UInt64(i as u64),
                                };
                            }
                        }
                    } else if left_type == DataType::Float64 && right_type == DataType::Int64 {
                        if let Expr::Literal(Value::Int64(i)) = right_bound {
                            right_bound = Expr::Literal(Value::Float64(i as f64));
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = right_bound
                        {
                            right_bound = Expr::Parameter {
                                index,
                                value: Value::Float64(i as f64),
                            };
                        }
                    } else if left_type == DataType::Int64 && right_type == DataType::Float64 {
                        if let Expr::Literal(Value::Int64(i)) = left_bound {
                            left_bound = Expr::Literal(Value::Float64(i as f64));
                        } else if let Expr::Parameter {
                            index,
                            value: Value::Int64(i),
                        } = left_bound
                        {
                            left_bound = Expr::Parameter {
                                index,
                                value: Value::Float64(i as f64),
                            };
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
                SqlBinaryOp::Add => BinaryOp::Add,
                SqlBinaryOp::Sub => BinaryOp::Sub,
                SqlBinaryOp::Mul => BinaryOp::Mul,
                SqlBinaryOp::Div => BinaryOp::Div,
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
        SqlExpr::SetSubquery(subquery) => {
            bind_set_subquery_internal(SubqueryKindTag::Scalar, subquery, ctx)
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
        SqlExpr::InSetSubquery {
            expr,
            subquery,
            negated,
        } => {
            let bound_expr = bind_expr(expr, ctx)?;
            bind_set_subquery_internal(
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
        SqlExpr::SetExists { subquery, negated } => {
            bind_set_subquery_internal(SubqueryKindTag::Exists { negated: *negated }, subquery, ctx)
        }
        SqlExpr::Window { .. } => Err(Error::SqlBindingError(
            "window expressions cannot be bound as scalar table expressions".to_string(),
        )),
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
        for we in &bound_select.window_exprs {
            grouped_columns.push(Column::new(we.display_name.clone(), we.output_type, true));
        }
        Schema::new_projected(grouped_columns)
    } else if !bound_select.window_exprs.is_empty() {
        let mut cols = inner_ctx.combined_schema.columns.clone();
        for we in &bound_select.window_exprs {
            cols.push(Column::new(we.display_name.clone(), we.output_type, true));
        }
        Schema::new_projected(cols)
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

pub(crate) fn unify_set_types(left: DataType, right: DataType) -> Option<DataType> {
    if left == right {
        Some(left)
    } else if left == DataType::Null {
        Some(right)
    } else if right == DataType::Null {
        Some(left)
    } else {
        match (left, right) {
            (DataType::Int64, DataType::UInt64) | (DataType::UInt64, DataType::Int64) => {
                Some(DataType::Int64)
            }
            (DataType::Int64, DataType::Float64)
            | (DataType::Float64, DataType::Int64)
            | (DataType::UInt64, DataType::Float64)
            | (DataType::Float64, DataType::UInt64) => Some(DataType::Float64),
            _ => None,
        }
    }
}

fn derive_plan_schema_in_binder(
    plan: &crate::query::optimizer::PhysicalPlan,
    table_schemas: &[(
        String,
        Option<String>,
        Schema,
        Vec<crate::engine::catalog::IndexMetadata>,
    )],
) -> Result<Schema> {
    if let Some(set_op) = &plan.set_op {
        return Ok(set_op.schema.clone());
    }

    let mut bindings = Vec::new();
    for (name, alias, schema, _) in table_schemas {
        bindings.push(TableBinding::new(name, alias.as_deref(), schema));
    }
    let ctx = BindingContext::new(bindings);

    let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();
    let schema_for_proj = if is_aggregate {
        let mut grouped_columns = Vec::new();
        for expr in &plan.group_by {
            let dt = expr.infer_type(&ctx.combined_schema)?;
            let col_name = match expr {
                Expr::Column(name) => name.clone(),
                _ => format!("{expr}"),
            };
            grouped_columns.push(Column::new(col_name, dt, true));
        }
        for agg in &plan.aggregates {
            let dt = agg.infer_type(&ctx.combined_schema)?;
            grouped_columns.push(Column::new(agg.display_name(), dt, true));
        }
        for we in &plan.window_exprs {
            grouped_columns.push(Column::new(we.display_name.clone(), we.output_type, true));
        }
        Schema::new_projected(grouped_columns)
    } else if !plan.window_exprs.is_empty() {
        let mut cols = ctx.combined_schema.columns.clone();
        for we in &plan.window_exprs {
            cols.push(Column::new(we.display_name.clone(), we.output_type, true));
        }
        Schema::new_projected(cols)
    } else {
        ctx.combined_schema.clone()
    };

    let mut out_cols = Vec::with_capacity(plan.projections.len());
    for (expr, alias) in &plan.projections {
        let dt = expr.infer_type(&schema_for_proj)?;
        let name = match alias {
            Some(a) => a.clone(),
            None => match expr {
                Expr::Column(col) => col.clone(),
                _ => format!("{expr}"),
            },
        };
        out_cols.push(Column::new(name, dt, true));
    }
    Ok(Schema::new_projected(out_cols))
}

fn plan_set_query_body_in_binder(
    body: &crate::sql::ast::SetQueryBody,
    outer_ctx: &BindingContext<'_>,
) -> Result<(crate::query::optimizer::PhysicalPlan, Schema)> {
    match body {
        crate::sql::ast::SetQueryBody::Select(statement) => {
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

            let inner_ctx = BindingContext::with_parent_and_catalog(
                inner_bindings,
                Some(outer_ctx),
                outer_ctx.catalog,
            );

            let bound_select = bind_select_with_context(statement, &inner_ctx)?;
            let inner_plan =
                crate::query::optimizer::Optimizer::optimize(&bound_select, &inner_opt_infos)?;
            let output_schema = derive_plan_schema_in_binder(&inner_plan, &table_schemas)?;
            Ok((inner_plan, output_schema))
        }
        crate::sql::ast::SetQueryBody::Parenthesized(inner) => {
            let (mut plan, schema) = plan_set_query_body_in_binder(&inner.body, outer_ctx)?;
            if let Some(limit) = inner.limit {
                plan.limit = Some(limit as usize);
            }
            if let Some(offset) = inner.offset {
                plan.offset = Some(offset as usize);
            }
            Ok((plan, schema))
        }
        crate::sql::ast::SetQueryBody::SetOperation { op, left, right } => {
            let (left_plan, left_schema) = plan_set_query_body_in_binder(left, outer_ctx)?;
            let (right_plan, right_schema) = plan_set_query_body_in_binder(right, outer_ctx)?;

            if left_schema.columns.len() != right_schema.columns.len() {
                return Err(Error::SetOperationColumnCountMismatch {
                    left_cols: left_schema.columns.len(),
                    right_cols: right_schema.columns.len(),
                });
            }

            let mut unified_cols = Vec::with_capacity(left_schema.columns.len());
            for (idx, (l_col, r_col)) in left_schema
                .columns
                .iter()
                .zip(right_schema.columns.iter())
                .enumerate()
            {
                let unified_type =
                    unify_set_types(l_col.data_type, r_col.data_type).ok_or_else(|| {
                        Error::SetOperationTypeMismatch {
                            col_idx: idx + 1,
                            left_type: format!("{:?}", l_col.data_type),
                            right_type: format!("{:?}", r_col.data_type),
                        }
                    })?;
                unified_cols.push(Column::new(
                    l_col.name.clone(),
                    unified_type,
                    l_col.nullable || r_col.nullable,
                ));
            }
            let unified_schema = Schema::new_projected(unified_cols);

            let plan = crate::query::optimizer::PhysicalPlan::new_set_op(
                crate::query::optimizer::PhysicalSetOp {
                    op: *op,
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    schema: unified_schema.clone(),
                },
            );

            Ok((plan, unified_schema))
        }
    }
}

fn bind_set_subquery_internal(
    kind_tag: SubqueryKindTag,
    set_query: &crate::sql::ast::SetQuery,
    outer_ctx: &BindingContext<'_>,
) -> Result<Expr> {
    let (mut plan, output_schema) = plan_set_query_body_in_binder(&set_query.body, outer_ctx)?;
    if let Some(limit) = set_query.limit {
        plan.limit = Some(limit as usize);
    }
    if let Some(offset) = set_query.offset {
        plan.offset = Some(offset as usize);
    }

    let subquery_kind = match kind_tag {
        SubqueryKindTag::Scalar => {
            if output_schema.columns.len() != 1 {
                return Err(Error::SubqueryColumnCountMismatch {
                    expected: 1,
                    found: output_schema.columns.len(),
                });
            }
            let data_type = output_schema.columns[0].data_type;
            crate::query::expr::SubqueryKind::Scalar { data_type }
        }
        SubqueryKindTag::In { expr, negated } => {
            if output_schema.columns.len() != 1 {
                return Err(Error::SubqueryColumnCountMismatch {
                    expected: 1,
                    found: output_schema.columns.len(),
                });
            }
            let left_type = expr.infer_type(&outer_ctx.combined_schema)?;
            let right_type = output_schema.columns[0].data_type;
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

    let mut outer_references = Vec::new();
    collect_outer_refs_from_plan(&plan, &mut outer_references);
    let is_correlated = !outer_references.is_empty();

    Ok(Expr::Subquery(Box::new(crate::query::expr::SubqueryExpr {
        kind: subquery_kind,
        statement: Box::new(SelectStatement {
            with: None,
            projection: vec![],
            from: crate::sql::ast::TableRef {
                name: String::new(),
                alias: None,
            },
            joins: vec![],
            selection: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
        }),
        plan,
        is_correlated,
        outer_references,
        materialized: None,
    })))
}

fn collect_outer_refs_from_plan(
    plan: &crate::query::optimizer::PhysicalPlan,
    refs: &mut Vec<String>,
) {
    if let Some(set_op) = &plan.set_op {
        collect_outer_refs_from_plan(&set_op.left, refs);
        collect_outer_refs_from_plan(&set_op.right, refs);
        return;
    }
    if let Some(rf) = &plan.base_table.residual_filter {
        collect_outer_refs_from_expr(rf, refs);
    }
    for j in &plan.joins {
        collect_outer_refs_from_expr(&j.on, refs);
        if let Some(rf) = &j.right.residual_filter {
            collect_outer_refs_from_expr(rf, refs);
        }
    }
    if let Some(post) = &plan.post_join_filter {
        collect_outer_refs_from_expr(post, refs);
    }
    if let Some(h) = &plan.having {
        collect_outer_refs_from_expr(h, refs);
    }
    for (p, _) in &plan.projections {
        collect_outer_refs_from_expr(p, refs);
    }
    for o in &plan.order_by {
        collect_outer_refs_from_expr(&o.expression, refs);
    }
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
        Expr::Literal(_) | Expr::Column(_) | Expr::Parameter { .. } => {}
    }
}
