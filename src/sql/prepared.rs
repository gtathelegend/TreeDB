use std::collections::HashMap;

use crate::engine::types::DataType;
use crate::error::Result;
use crate::sql::ast::{
    CteQuery, DeleteStatement, InsertStatement, SelectItem, SelectStatement, SetQuery,
    SetQueryBody, SqlExpr, SqlLiteral, Statement, UpdateStatement,
};
use crate::sql::binder::TableCatalog;

/// Metadata describing a single positional parameter in a prepared statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterMetadata {
    /// 1-indexed positional identifier (e.g. 1 for first ?, 2 for second ?).
    pub index: usize,
    /// Inferred SQL data type from context if resolvable, or `None` if ambiguous (e.g. `SELECT ?`).
    pub inferred_type: Option<DataType>,
}

impl ParameterMetadata {
    /// Creates a new `ParameterMetadata`.
    pub fn new(index: usize, inferred_type: Option<DataType>) -> Self {
        Self {
            index,
            inferred_type,
        }
    }
}

/// A parsed, reusable prepared SQL statement with parameter metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedStatement {
    /// Original SQL string.
    pub sql: String,
    /// Parsed AST statement.
    pub statement: Statement,
    /// Positional parameter metadata in ascending 1-indexed order.
    pub parameters: Vec<ParameterMetadata>,
}

impl PreparedStatement {
    /// Creates a new `PreparedStatement`.
    pub fn new(
        sql: impl Into<String>,
        statement: Statement,
        parameters: Vec<ParameterMetadata>,
    ) -> Self {
        Self {
            sql: sql.into(),
            statement,
            parameters,
        }
    }

    /// Returns the number of positional parameters in the prepared statement.
    pub fn parameter_count(&self) -> usize {
        self.parameters.len()
    }

    /// Returns the parameter metadata slice.
    pub fn parameters(&self) -> &[ParameterMetadata] {
        &self.parameters
    }

    /// Returns metadata for the parameter at the given 1-indexed position.
    pub fn parameter(&self, index: usize) -> Option<&ParameterMetadata> {
        if index == 0 || index > self.parameters.len() {
            None
        } else {
            Some(&self.parameters[index - 1])
        }
    }

    /// Returns the original SQL text.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Returns the parsed AST statement.
    pub fn statement(&self) -> &Statement {
        &self.statement
    }
}

/// Scope context used for resolving identifier types during parameter type inference.
#[derive(Debug, Clone)]
struct InferScope<'a> {
    columns: Vec<(Option<String>, String, DataType)>,
    parent: Option<&'a InferScope<'a>>,
}

impl<'a> InferScope<'a> {
    fn new(columns: Vec<(Option<String>, String, DataType)>) -> Self {
        Self {
            columns,
            parent: None,
        }
    }

    fn with_parent(
        columns: Vec<(Option<String>, String, DataType)>,
        parent: Option<&'a InferScope<'a>>,
    ) -> Self {
        Self { columns, parent }
    }

    fn find_column(&self, qualifier: Option<&str>, name: &str) -> Option<DataType> {
        if let Some(q) = qualifier {
            for (tbl_q, col_name, dt) in &self.columns {
                if col_name.eq_ignore_ascii_case(name) {
                    if let Some(t_q) = tbl_q {
                        if t_q.eq_ignore_ascii_case(q) {
                            return Some(*dt);
                        }
                    }
                }
            }
        } else {
            for (tbl_q, col_name, dt) in &self.columns {
                if col_name.eq_ignore_ascii_case(name) && tbl_q.is_none() {
                    return Some(*dt);
                }
            }
            for (_, col_name, dt) in &self.columns {
                if col_name.eq_ignore_ascii_case(name) {
                    return Some(*dt);
                }
            }
        }

        if let Some(p) = self.parent {
            p.find_column(qualifier, name)
        } else {
            None
        }
    }
}

/// Analyzes an AST `Statement` against the catalog to extract positional parameters
/// and infer parameter types from their surrounding SQL context.
pub fn infer_statement_parameters(
    stmt: &Statement,
    catalog: &dyn TableCatalog,
) -> Result<Vec<ParameterMetadata>> {
    let mut inferred_types: HashMap<usize, DataType> = HashMap::new();
    let mut max_param_index = 0usize;

    collect_and_infer_stmt(stmt, catalog, &mut inferred_types, &mut max_param_index)?;

    let mut result = Vec::with_capacity(max_param_index);
    for idx in 1..=max_param_index {
        let inferred_type = inferred_types.get(&idx).copied();
        result.push(ParameterMetadata::new(idx, inferred_type));
    }

    Ok(result)
}

fn collect_and_infer_stmt(
    stmt: &Statement,
    catalog: &dyn TableCatalog,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    match stmt {
        Statement::Select(select_stmt) => {
            infer_select(select_stmt, catalog, None, inferred, max_idx)?;
        }
        Statement::SetQuery(set_stmt) => {
            infer_set_query(set_stmt, catalog, None, inferred, max_idx)?;
        }
        Statement::Insert(insert_stmt) => {
            infer_insert(insert_stmt, catalog, inferred, max_idx)?;
        }
        Statement::Update(update_stmt) => {
            infer_update(update_stmt, catalog, inferred, max_idx)?;
        }
        Statement::Delete(delete_stmt) => {
            infer_delete(delete_stmt, catalog, inferred, max_idx)?;
        }
        Statement::Explain(explain_stmt) => {
            collect_and_infer_stmt(&explain_stmt.query, catalog, inferred, max_idx)?;
        }
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Analyze(_) => {}
    }
    Ok(())
}

fn infer_select(
    stmt: &SelectStatement,
    catalog: &dyn TableCatalog,
    parent_scope: Option<&InferScope<'_>>,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    // 1. Resolve tables and schemas
    let mut columns = Vec::new();
    if let Ok(from_schema) = catalog.get_table_schema(&stmt.from.name) {
        for col in &from_schema.columns {
            columns.push((
                Some(stmt.from.name.clone()),
                col.name.clone(),
                col.data_type,
            ));
            if let Some(alias) = &stmt.from.alias {
                columns.push((Some(alias.clone()), col.name.clone(), col.data_type));
            }
            columns.push((None, col.name.clone(), col.data_type));
        }
    }

    for join in &stmt.joins {
        if let Ok(join_schema) = catalog.get_table_schema(&join.table.name) {
            for col in &join_schema.columns {
                columns.push((
                    Some(join.table.name.clone()),
                    col.name.clone(),
                    col.data_type,
                ));
                if let Some(alias) = &join.table.alias {
                    columns.push((Some(alias.clone()), col.name.clone(), col.data_type));
                }
                columns.push((None, col.name.clone(), col.data_type));
            }
        }
    }

    let scope = InferScope::with_parent(columns, parent_scope);

    // 2. CTEs
    if let Some(with) = &stmt.with {
        for cte in &with.ctes {
            match &cte.query {
                CteQuery::NonRecursive(sel) => {
                    infer_select(sel, catalog, Some(&scope), inferred, max_idx)?;
                }
                CteQuery::SetOperation(set) => {
                    infer_set_query(set, catalog, Some(&scope), inferred, max_idx)?;
                }
                CteQuery::Recursive { anchor, recursive } => {
                    infer_select(anchor, catalog, Some(&scope), inferred, max_idx)?;
                    infer_select(recursive, catalog, Some(&scope), inferred, max_idx)?;
                }
            }
        }
    }

    // 3. Projections
    for item in &stmt.projection {
        if let SelectItem::Expression { expr, .. } = item {
            infer_expr(expr, &scope, catalog, inferred, max_idx)?;
        }
    }

    // 4. Joins
    for join in &stmt.joins {
        infer_expr(&join.on, &scope, catalog, inferred, max_idx)?;
    }

    // 5. WHERE
    if let Some(sel) = &stmt.selection {
        infer_expr(sel, &scope, catalog, inferred, max_idx)?;
    }

    // 6. GROUP BY & HAVING
    for gb in &stmt.group_by {
        infer_expr(gb, &scope, catalog, inferred, max_idx)?;
    }
    if let Some(having) = &stmt.having {
        infer_expr(having, &scope, catalog, inferred, max_idx)?;
    }

    // 7. ORDER BY
    for ob in &stmt.order_by {
        infer_expr(&ob.expr, &scope, catalog, inferred, max_idx)?;
    }

    Ok(())
}

fn infer_set_query(
    stmt: &SetQuery,
    catalog: &dyn TableCatalog,
    parent_scope: Option<&InferScope<'_>>,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    if let Some(with) = &stmt.with {
        for cte in &with.ctes {
            match &cte.query {
                CteQuery::NonRecursive(sel) => {
                    infer_select(sel, catalog, parent_scope, inferred, max_idx)?;
                }
                CteQuery::SetOperation(set) => {
                    infer_set_query(set, catalog, parent_scope, inferred, max_idx)?;
                }
                CteQuery::Recursive { anchor, recursive } => {
                    infer_select(anchor, catalog, parent_scope, inferred, max_idx)?;
                    infer_select(recursive, catalog, parent_scope, inferred, max_idx)?;
                }
            }
        }
    }

    infer_set_query_body(&stmt.body, catalog, parent_scope, inferred, max_idx)?;

    for ob in &stmt.order_by {
        let dummy_scope = InferScope::new(Vec::new());
        infer_expr(&ob.expr, &dummy_scope, catalog, inferred, max_idx)?;
    }

    Ok(())
}

fn infer_set_query_body(
    body: &SetQueryBody,
    catalog: &dyn TableCatalog,
    parent_scope: Option<&InferScope<'_>>,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    match body {
        SetQueryBody::Select(sel) => {
            infer_select(sel, catalog, parent_scope, inferred, max_idx)?;
        }
        SetQueryBody::Parenthesized(inner) => {
            infer_set_query(inner, catalog, parent_scope, inferred, max_idx)?;
        }
        SetQueryBody::SetOperation { left, right, .. } => {
            infer_set_query_body(left, catalog, parent_scope, inferred, max_idx)?;
            infer_set_query_body(right, catalog, parent_scope, inferred, max_idx)?;
        }
    }
    Ok(())
}

fn infer_insert(
    stmt: &InsertStatement,
    catalog: &dyn TableCatalog,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    let schema_opt = catalog.get_table_schema(&stmt.table_name).ok();
    if let Some(schema) = schema_opt {
        let col_indices: Vec<usize> = if let Some(col_names) = &stmt.columns {
            let mut indices = Vec::new();
            for name in col_names {
                if let Some(pos) = schema.columns.iter().position(|c| &c.name == name) {
                    indices.push(pos);
                }
            }
            indices
        } else {
            (0..schema.columns.len()).collect()
        };

        let mut columns = Vec::new();
        for col in &schema.columns {
            columns.push((
                Some(stmt.table_name.clone()),
                col.name.clone(),
                col.data_type,
            ));
            columns.push((None, col.name.clone(), col.data_type));
        }
        let scope = InferScope::new(columns);

        for row in &stmt.values {
            for (i, expr) in row.iter().enumerate() {
                if i < col_indices.len() {
                    let target_col_idx = col_indices[i];
                    let target_type = schema.columns[target_col_idx].data_type;
                    if let SqlExpr::Parameter(p_idx) = expr {
                        *max_idx = (*max_idx).max(*p_idx);
                        inferred.insert(*p_idx, target_type);
                    } else {
                        infer_expr(expr, &scope, catalog, inferred, max_idx)?;
                    }
                } else {
                    infer_expr(expr, &scope, catalog, inferred, max_idx)?;
                }
            }
        }
    } else {
        let dummy_scope = InferScope::new(Vec::new());
        for row in &stmt.values {
            for expr in row {
                infer_expr(expr, &dummy_scope, catalog, inferred, max_idx)?;
            }
        }
    }
    Ok(())
}

fn infer_update(
    stmt: &UpdateStatement,
    catalog: &dyn TableCatalog,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    let schema_opt = catalog.get_table_schema(&stmt.table_name).ok();
    if let Some(schema) = schema_opt {
        let mut columns = Vec::new();
        for col in &schema.columns {
            columns.push((
                Some(stmt.table_name.clone()),
                col.name.clone(),
                col.data_type,
            ));
            columns.push((None, col.name.clone(), col.data_type));
        }
        let scope = InferScope::new(columns);

        for assign in &stmt.assignments {
            if let Some(col) = schema.columns.iter().find(|c| c.name == assign.column_name) {
                if let SqlExpr::Parameter(p_idx) = &assign.expr {
                    *max_idx = (*max_idx).max(*p_idx);
                    inferred.insert(*p_idx, col.data_type);
                } else {
                    infer_expr(&assign.expr, &scope, catalog, inferred, max_idx)?;
                }
            } else {
                infer_expr(&assign.expr, &scope, catalog, inferred, max_idx)?;
            }
        }
        if let Some(sel) = &stmt.selection {
            infer_expr(sel, &scope, catalog, inferred, max_idx)?;
        }
    } else {
        let dummy_scope = InferScope::new(Vec::new());
        for assign in &stmt.assignments {
            infer_expr(&assign.expr, &dummy_scope, catalog, inferred, max_idx)?;
        }
        if let Some(sel) = &stmt.selection {
            infer_expr(sel, &dummy_scope, catalog, inferred, max_idx)?;
        }
    }
    Ok(())
}

fn infer_delete(
    stmt: &DeleteStatement,
    catalog: &dyn TableCatalog,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    let schema_opt = catalog.get_table_schema(&stmt.table_name).ok();
    if let Some(schema) = schema_opt {
        let mut columns = Vec::new();
        for col in &schema.columns {
            columns.push((
                Some(stmt.table_name.clone()),
                col.name.clone(),
                col.data_type,
            ));
            columns.push((None, col.name.clone(), col.data_type));
        }
        let scope = InferScope::new(columns);

        if let Some(sel) = &stmt.selection {
            infer_expr(sel, &scope, catalog, inferred, max_idx)?;
        }
    } else {
        let dummy_scope = InferScope::new(Vec::new());
        if let Some(sel) = &stmt.selection {
            infer_expr(sel, &dummy_scope, catalog, inferred, max_idx)?;
        }
    }
    Ok(())
}

fn resolve_sql_expr_type(expr: &SqlExpr, scope: &InferScope<'_>) -> Option<DataType> {
    match expr {
        SqlExpr::Literal(lit) => match lit {
            SqlLiteral::Null => Some(DataType::Null),
            SqlLiteral::Boolean(_) => Some(DataType::Boolean),
            SqlLiteral::Integer(_) => Some(DataType::Int64),
            SqlLiteral::Float(_) => Some(DataType::Float64),
            SqlLiteral::String(_) => Some(DataType::String),
        },
        SqlExpr::Identifier(col_name) => scope.find_column(None, col_name),
        SqlExpr::QualifiedIdentifier { qualifier, name } => {
            scope.find_column(Some(qualifier), name)
        }
        SqlExpr::Binary { left, op, right } => {
            let lt = resolve_sql_expr_type(left, scope);
            let rt = resolve_sql_expr_type(right, scope);
            match op {
                crate::sql::ast::SqlBinaryOp::Eq
                | crate::sql::ast::SqlBinaryOp::NotEq
                | crate::sql::ast::SqlBinaryOp::Lt
                | crate::sql::ast::SqlBinaryOp::LtEq
                | crate::sql::ast::SqlBinaryOp::Gt
                | crate::sql::ast::SqlBinaryOp::GtEq
                | crate::sql::ast::SqlBinaryOp::And
                | crate::sql::ast::SqlBinaryOp::Or => Some(DataType::Boolean),
                crate::sql::ast::SqlBinaryOp::Add
                | crate::sql::ast::SqlBinaryOp::Sub
                | crate::sql::ast::SqlBinaryOp::Mul
                | crate::sql::ast::SqlBinaryOp::Div => {
                    if lt == Some(DataType::Float64) || rt == Some(DataType::Float64) {
                        Some(DataType::Float64)
                    } else if lt == Some(DataType::UInt64) && rt == Some(DataType::UInt64) {
                        Some(DataType::UInt64)
                    } else {
                        lt.or(rt)
                    }
                }
            }
        }
        SqlExpr::IsNull(_) | SqlExpr::IsNotNull(_) => Some(DataType::Boolean),
        SqlExpr::CountAll => Some(DataType::UInt64),
        SqlExpr::Aggregate { function, arg } => match function {
            crate::sql::ast::AggregateFunction::Count => Some(DataType::UInt64),
            crate::sql::ast::AggregateFunction::Avg => Some(DataType::Float64),
            crate::sql::ast::AggregateFunction::Sum
            | crate::sql::ast::AggregateFunction::Min
            | crate::sql::ast::AggregateFunction::Max => resolve_sql_expr_type(arg, scope),
        },
        _ => None,
    }
}

fn infer_expr(
    expr: &SqlExpr,
    scope: &InferScope<'_>,
    catalog: &dyn TableCatalog,
    inferred: &mut HashMap<usize, DataType>,
    max_idx: &mut usize,
) -> Result<()> {
    match expr {
        SqlExpr::Parameter(p_idx) => {
            *max_idx = (*max_idx).max(*p_idx);
        }
        SqlExpr::Binary { left, right, .. } => {
            let lt = resolve_sql_expr_type(left, scope);
            let rt = resolve_sql_expr_type(right, scope);

            if let SqlExpr::Parameter(p_idx) = left.as_ref() {
                *max_idx = (*max_idx).max(*p_idx);
                if let Some(t) = rt {
                    if t != DataType::Null {
                        inferred.insert(*p_idx, t);
                    }
                }
            }
            if let SqlExpr::Parameter(p_idx) = right.as_ref() {
                *max_idx = (*max_idx).max(*p_idx);
                if let Some(t) = lt {
                    if t != DataType::Null {
                        inferred.insert(*p_idx, t);
                    }
                }
            }

            infer_expr(left, scope, catalog, inferred, max_idx)?;
            infer_expr(right, scope, catalog, inferred, max_idx)?;
        }
        SqlExpr::Unary { expr, .. } | SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
            infer_expr(expr, scope, catalog, inferred, max_idx)?;
        }
        SqlExpr::InList { expr, list, .. } => {
            let expr_type = resolve_sql_expr_type(expr, scope);
            if let SqlExpr::Parameter(p_idx) = expr.as_ref() {
                *max_idx = (*max_idx).max(*p_idx);
                for item in list {
                    if let Some(t) = resolve_sql_expr_type(item, scope) {
                        if t != DataType::Null {
                            inferred.insert(*p_idx, t);
                            break;
                        }
                    }
                }
            } else if let Some(t) = expr_type {
                if t != DataType::Null {
                    for item in list {
                        if let SqlExpr::Parameter(p_idx) = item {
                            *max_idx = (*max_idx).max(*p_idx);
                            inferred.insert(*p_idx, t);
                        }
                    }
                }
            }

            infer_expr(expr, scope, catalog, inferred, max_idx)?;
            for item in list {
                infer_expr(item, scope, catalog, inferred, max_idx)?;
            }
        }
        SqlExpr::Aggregate { arg, .. } => {
            infer_expr(arg, scope, catalog, inferred, max_idx)?;
        }
        SqlExpr::Window {
            args, window_spec, ..
        } => {
            for arg in args {
                infer_expr(arg, scope, catalog, inferred, max_idx)?;
            }
            for p in &window_spec.partition_by {
                infer_expr(p, scope, catalog, inferred, max_idx)?;
            }
            for o in &window_spec.order_by {
                infer_expr(&o.expr, scope, catalog, inferred, max_idx)?;
            }
        }
        SqlExpr::Subquery(sub) => {
            infer_select(sub, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::SetSubquery(sub) => {
            infer_set_query(sub, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::Exists { subquery, .. } => {
            infer_select(subquery, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::SetExists { subquery, .. } => {
            infer_set_query(subquery, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::InSubquery { expr, subquery, .. } => {
            infer_expr(expr, scope, catalog, inferred, max_idx)?;
            infer_select(subquery, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::InSetSubquery { expr, subquery, .. } => {
            infer_expr(expr, scope, catalog, inferred, max_idx)?;
            infer_set_query(subquery, catalog, Some(scope), inferred, max_idx)?;
        }
        SqlExpr::Literal(_)
        | SqlExpr::Identifier(_)
        | SqlExpr::QualifiedIdentifier { .. }
        | SqlExpr::CountAll => {}
    }
    Ok(())
}
