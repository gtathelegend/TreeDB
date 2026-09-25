use crate::engine::types::{DataType, Value};
use crate::engine::TreeDb;
use crate::error::{Error, Result};
use crate::storage::page::{Key, PageId};
use crate::wal::lsn::TxnId;

/// Execution result returned by top-level SQL statements (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`).
#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    /// Result of an SQL `SELECT` query.
    Query(crate::query::QueryResult),
    /// Result of an SQL `INSERT` statement.
    Inserted {
        /// Number of rows successfully inserted.
        rows: usize,
    },
    /// Result of an SQL `UPDATE` statement.
    Updated {
        /// Number of rows successfully updated.
        rows: usize,
    },
    /// Result of an SQL `DELETE` statement.
    Deleted {
        /// Number of rows successfully deleted.
        rows: usize,
    },
    /// Result of an SQL `CREATE TABLE` statement.
    CreatedTable {
        /// Name of the created table.
        table: String,
    },
    /// Result of an SQL `DROP TABLE` statement.
    DroppedTable {
        /// Name of the dropped table.
        table: String,
    },
}

impl StatementResult {
    /// Unwraps this result as a `QueryResult`, returning an error if it is not a query.
    pub fn into_query(self) -> Result<crate::query::QueryResult> {
        match self {
            StatementResult::Query(qr) => Ok(qr),
            other => Err(Error::QueryExecutionError(format!(
                "expected query result, found statement result: {other:?}"
            ))),
        }
    }

    /// Returns the number of rows affected by an `INSERT`, `UPDATE`, or `DELETE` statement,
    /// or the number of rows returned by a `SELECT` query.
    pub fn rows_affected(&self) -> usize {
        match self {
            StatementResult::Query(qr) => qr.rows.len(),
            StatementResult::Inserted { rows }
            | StatementResult::Updated { rows }
            | StatementResult::Deleted { rows } => *rows,
            StatementResult::CreatedTable { .. } | StatementResult::DroppedTable { .. } => 0,
        }
    }
}

/// State of an explicit transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// Transaction is currently active and can accept reads and mutations.
    Active,
    /// Transaction has successfully committed.
    Committed,
    /// Transaction has been aborted and rolled back.
    Aborted,
}

/// An explicit atomic transaction handle coordinating operations across the B+ Tree index,
/// Record Store, and Buffer Pool within a single WAL transaction context (`TxnId`).
#[derive(Debug)]
pub struct Transaction<'db> {
    db: &'db mut TreeDb,
    txn_id: TxnId,
    state: TxnState,
    initial_root_page_id: PageId,
    initial_freelist_head: PageId,
    initial_free_page_count: u32,
    initial_page_count: u32,
}

impl<'db> Transaction<'db> {
    pub(crate) fn new(
        db: &'db mut TreeDb,
        txn_id: TxnId,
        initial_root_page_id: PageId,
        initial_freelist_head: PageId,
        initial_free_page_count: u32,
        initial_page_count: u32,
    ) -> Self {
        Self {
            db,
            txn_id,
            state: TxnState::Active,
            initial_root_page_id,
            initial_freelist_head,
            initial_free_page_count,
            initial_page_count,
        }
    }

    /// Returns the unique transaction identifier.
    pub const fn id(&self) -> TxnId {
        self.txn_id
    }

    /// Returns the current lifecycle state of the transaction.
    pub const fn state(&self) -> TxnState {
        self.state
    }

    /// Returns true if the transaction is in the active state.
    pub const fn is_active(&self) -> bool {
        matches!(self.state, TxnState::Active)
    }

    /// Retrieves an owned copy of the value associated with `key` within this transaction.
    ///
    /// Observes prior mutations (inserts, updates, deletes) made within the same transaction.
    pub fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.ensure_active()?;
        match self.db.tree.get(&mut self.db.buffer_pool, key)? {
            Some(record_id) => self
                .db
                .record_store
                .get(&mut self.db.buffer_pool, record_id),
            None => Ok(None),
        }
    }

    /// Inserts or updates a key-value mapping within this transaction.
    pub fn put(&mut self, key: Key, value: &[u8]) -> Result<()> {
        self.ensure_active()?;
        if let Some(old_rid) = self.db.tree.get(&mut self.db.buffer_pool, key)? {
            let record_id = self
                .db
                .record_store
                .insert(&mut self.db.buffer_pool, value)?;
            self.db
                .tree
                .update(&mut self.db.buffer_pool, key, record_id)?;
            self.db
                .record_store
                .delete(&mut self.db.buffer_pool, old_rid)?;
        } else {
            let record_id = self
                .db
                .record_store
                .insert(&mut self.db.buffer_pool, value)?;
            self.db
                .tree
                .insert(&mut self.db.buffer_pool, key, record_id)?;
        }
        Ok(())
    }

    /// Updates an existing key-value mapping within this transaction.
    pub fn update(&mut self, key: Key, value: &[u8]) -> Result<()> {
        self.put(key, value)
    }

    /// Deletes a key and its associated value within this transaction.
    ///
    /// Returns `Ok(Some(Vec<u8>))` with the deleted value bytes if present, or `Ok(None)` if absent.
    pub fn delete(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.ensure_active()?;
        match self.db.tree.delete(&mut self.db.buffer_pool, key)? {
            Some(record_id) => {
                let value = self
                    .db
                    .record_store
                    .get(&mut self.db.buffer_pool, record_id)?;
                self.db
                    .record_store
                    .delete(&mut self.db.buffer_pool, record_id)?;
                Ok(value)
            }
            None => Ok(None),
        }
    }

    /// Performs an ordered range scan over keys in `[start, end)` within this transaction.
    pub fn scan(&mut self, start: Option<Key>, end: Option<Key>) -> Result<Vec<(Key, Vec<u8>)>> {
        self.ensure_active()?;
        let rids = self.db.tree.scan(&mut self.db.buffer_pool, start, end)?;
        let mut results = Vec::with_capacity(rids.len());

        for (k, rid) in rids {
            if let Some(val) = self.db.record_store.get(&mut self.db.buffer_pool, rid)? {
                results.push((k, val));
            }
        }

        Ok(results)
    }

    /// Creates a new table in the catalog within this transaction.
    pub fn create_table(
        &mut self,
        name: &str,
        schema: crate::engine::types::Schema,
    ) -> Result<crate::engine::catalog::TableId> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::create_table(&mut self.db.buffer_pool, name, schema)
    }

    /// Opens an existing table handle for relational operations within this transaction.
    pub fn open_table(&mut self, name: &str) -> Result<crate::engine::table::Table<'_>> {
        self.ensure_active()?;
        let metadata = crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, name)?
            .ok_or_else(|| Error::TableNotFound(name.to_string()))?;
        let tree = crate::index::bplus_tree::BPlusTree::from_root(metadata.root_page_id);
        let record_store =
            crate::storage::record_store::RecordStore::with_last_page(metadata.record_page_id);
        let index_metas = crate::engine::catalog::Catalog::list_indexes_for_table(
            &mut self.db.buffer_pool,
            metadata.table_id,
        )?;
        let mut indexes = Vec::with_capacity(index_metas.len());
        for idx_meta in index_metas {
            let sec_tree =
                crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
            indexes.push((idx_meta, sec_tree));
        }

        Ok(crate::engine::table::Table::new(
            metadata,
            tree,
            record_store,
            indexes,
            &mut self.db.buffer_pool,
        ))
    }

    /// Creates a new persistent secondary index within this transaction.
    pub fn create_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_name: &str,
        unique: bool,
    ) -> Result<crate::engine::catalog::IndexId> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::create_index(
            &mut self.db.buffer_pool,
            table_name,
            index_name,
            column_name,
            unique,
        )
    }

    /// Drops a persistent secondary index and reclaims its pages within this transaction.
    pub fn drop_index(&mut self, index_name: &str) -> Result<()> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::drop_index(&mut self.db.buffer_pool, index_name)
    }

    /// Retrieves metadata for a secondary index by name.
    pub fn get_index(
        &mut self,
        name: &str,
    ) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, name)
    }

    /// Retrieves metadata for a secondary index by `IndexId`.
    pub fn get_index_by_id(
        &mut self,
        id: crate::engine::catalog::IndexId,
    ) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_index_by_id(&mut self.db.buffer_pool, id)
    }

    /// Lists all secondary indexes registered in the catalog.
    pub fn list_indexes(&mut self) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::list_indexes(&mut self.db.buffer_pool)
    }

    /// Lists all secondary indexes registered for a specific table.
    pub fn list_indexes_for_table(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        let table = self
            .get_table(table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        crate::engine::catalog::Catalog::list_indexes_for_table(
            &mut self.db.buffer_pool,
            table.table_id,
        )
    }

    /// Performs a secondary index lookup returning matching `RecordId`s.
    pub fn index_lookup(
        &mut self,
        table_name: &str,
        index_name: &str,
        value: &crate::engine::types::Value,
    ) -> Result<Vec<crate::index::node::RecordId>> {
        self.ensure_active()?;
        let idx_meta =
            crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, index_name)?
                .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;
        let table =
            crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, table_name)?
                .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        if idx_meta.table_id != table.table_id {
            return Err(Error::IndexTableMismatch {
                index: index_name.to_string(),
                table: table_name.to_string(),
            });
        }
        let sec_tree = crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
        let idx_key = crate::index::secondary::IndexKey::from_value(value);
        sec_tree.lookup(&mut self.db.buffer_pool, &idx_key)
    }

    /// Performs a secondary index range scan returning matching `(Value, RecordId)` pairs.
    pub fn index_scan(
        &mut self,
        table_name: &str,
        index_name: &str,
        start: Option<&crate::engine::types::Value>,
        end: Option<&crate::engine::types::Value>,
    ) -> Result<Vec<(crate::engine::types::Value, crate::index::node::RecordId)>> {
        self.ensure_active()?;
        let idx_meta =
            crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, index_name)?
                .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;
        let table =
            crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, table_name)?
                .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        if idx_meta.table_id != table.table_id {
            return Err(Error::IndexTableMismatch {
                index: index_name.to_string(),
                table: table_name.to_string(),
            });
        }
        let col_type = table.schema.columns[idx_meta.column_idx].data_type;
        let sec_tree = crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);

        let start_key = start.map(crate::index::secondary::IndexKey::from_value);
        let end_key = end.map(crate::index::secondary::IndexKey::from_value);

        let entries = sec_tree.scan(
            &mut self.db.buffer_pool,
            start_key.as_ref(),
            end_key.as_ref(),
        )?;
        let mut results = Vec::with_capacity(entries.len());
        for (k, rid) in entries {
            let val = k.to_value(col_type)?;
            results.push((val, rid));
        }
        Ok(results)
    }

    /// Validates the structural invariants of a named secondary index.
    pub fn validate_index(&mut self, index_name: &str) -> Result<()> {
        self.ensure_active()?;
        let idx_meta =
            crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, index_name)?
                .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;
        let sec_tree = crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
        sec_tree.validate_tree(&mut self.db.buffer_pool, idx_meta.unique)
    }

    /// Constructs a programmatic query against a named table within this transaction.
    pub fn query(&mut self, table_name: &str) -> Result<crate::query::Query<'_>> {
        let table = self.open_table(table_name)?;
        Ok(crate::query::Query::new(table))
    }

    /// Plans a parsed `SelectStatement` AST into an optimized physical execution plan (`PhysicalPlan`).
    pub fn plan_select(
        &mut self,
        ast: &crate::sql::SelectStatement,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let mut table_infos = Vec::new();
        let mut bindings = Vec::new();

        {
            let base_table = self.open_table(&ast.from.name)?;
            let schema = base_table.schema().clone();
            let indexes = base_table
                .indexes()
                .iter()
                .map(|(m, _)| m.clone())
                .collect();
            table_infos.push(crate::query::optimizer::TableOptInfo::new(
                &ast.from.name,
                ast.from.alias.as_deref(),
                schema,
                indexes,
            ));
        }

        for j in &ast.joins {
            let t = self.open_table(&j.table.name)?;
            let schema = t.schema().clone();
            let indexes = t.indexes().iter().map(|(m, _)| m.clone()).collect();
            table_infos.push(crate::query::optimizer::TableOptInfo::new(
                &j.table.name,
                j.table.alias.as_deref(),
                schema,
                indexes,
            ));
        }

        for t in &table_infos {
            bindings.push(crate::sql::TableBinding::new(
                &t.name,
                t.alias.as_deref(),
                &t.schema,
            ));
        }

        let bound = crate::sql::bind_select_multi(ast, &bindings)?;
        crate::query::optimizer::Optimizer::optimize(&bound, &table_infos)
    }

    /// Parses, binds, and produces an optimized physical execution plan (`PhysicalPlan`) for the given SQL query or DML filter.
    pub fn plan_sql(&mut self, sql: &str) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => self.plan_select(&select_stmt),
            crate::sql::Statement::Update(update_stmt) => {
                let select_ast = crate::sql::SelectStatement {
                    projection: vec![crate::sql::SelectItem::Wildcard],
                    from: crate::sql::TableRef::new(&update_stmt.table_name),
                    joins: Vec::new(),
                    selection: update_stmt.selection,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                self.plan_select(&select_ast)
            }
            crate::sql::Statement::Delete(delete_stmt) => {
                let select_ast = crate::sql::SelectStatement {
                    projection: vec![crate::sql::SelectItem::Wildcard],
                    from: crate::sql::TableRef::new(&delete_stmt.table_name),
                    joins: Vec::new(),
                    selection: delete_stmt.selection,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                self.plan_select(&select_ast)
            }
            _ => Err(Error::QueryExecutionError(format!(
                "cannot plan statement: {stmt}"
            ))),
        }
    }

    /// Generates a human-readable text explanation of the optimized physical execution plan for the given SQL query.
    pub fn explain_sql(&mut self, sql: &str) -> Result<String> {
        let plan = self.plan_sql(sql)?;
        Ok(plan.to_string())
    }

    /// Executes an arbitrary top-level SQL statement (`SELECT`, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`)
    /// within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_statement(&mut self, sql: &str) -> Result<StatementResult> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                let query_result = self.execute_physical_plan(&plan)?;
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::CreateTable(create_stmt) => {
                self.execute_create_table(&create_stmt)
            }
            crate::sql::Statement::DropTable(drop_stmt) => self.execute_drop_table(&drop_stmt),
            crate::sql::Statement::Insert(insert_stmt) => self.execute_insert(&insert_stmt),
            crate::sql::Statement::Update(update_stmt) => self.execute_update(&update_stmt),
            crate::sql::Statement::Delete(delete_stmt) => self.execute_delete(&delete_stmt),
        }
    }

    /// Parses, binds, optimizes, and executes an SQL `SELECT` statement within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_sql(&mut self, sql: &str) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;
        let res = self.execute_statement(sql)?;
        res.into_query()
    }

    fn execute_create_table(
        &mut self,
        stmt: &crate::sql::CreateTableStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let mut columns = Vec::with_capacity(stmt.columns.len());
        let mut pk_idx = None;

        for (i, col_def) in stmt.columns.iter().enumerate() {
            if col_def.is_primary_key {
                pk_idx = Some(i);
            }
            columns.push(crate::engine::types::Column::new(
                &col_def.name,
                col_def.data_type,
                col_def.nullable,
            ));
        }

        let pk_index = pk_idx.ok_or_else(|| {
            Error::TableSchemaMismatch("table must define a primary key column".to_string())
        })?;

        let schema = crate::engine::types::Schema::new(columns, pk_index)?;
        self.create_table(&stmt.table_name, schema)?;
        Ok(StatementResult::CreatedTable {
            table: stmt.table_name.clone(),
        })
    }

    fn execute_drop_table(
        &mut self,
        stmt: &crate::sql::DropTableStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        self.drop_table(&stmt.table_name)?;
        Ok(StatementResult::DroppedTable {
            table: stmt.table_name.clone(),
        })
    }

    fn execute_insert(&mut self, stmt: &crate::sql::InsertStatement) -> Result<StatementResult> {
        self.ensure_active()?;
        let mut table = self.open_table(&stmt.table_name)?;
        let schema = table.schema().clone();

        // 1. Resolve column mapping
        let col_indices: Vec<usize> = if let Some(col_names) = &stmt.columns {
            let mut indices = Vec::with_capacity(col_names.len());
            let mut seen = std::collections::HashSet::new();
            for name in col_names {
                if !seen.insert(name) {
                    return Err(Error::TableSchemaMismatch(format!(
                        "duplicate column '{name}' in INSERT column list"
                    )));
                }
                let idx = schema
                    .columns
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
                indices.push(idx);
            }
            indices
        } else {
            (0..schema.columns.len()).collect()
        };

        // 2. Validate and assemble each row
        let mut rows_to_insert = Vec::with_capacity(stmt.values.len());
        let empty_row = crate::engine::types::Row::new(Vec::new());

        for value_tuple in &stmt.values {
            if value_tuple.len() != col_indices.len() {
                return Err(Error::ColumnCountMismatch {
                    expected: col_indices.len(),
                    found: value_tuple.len(),
                });
            }

            let mut row_values = vec![Value::Null; schema.columns.len()];
            let mut populated = vec![false; schema.columns.len()];

            for (expr, &target_idx) in value_tuple.iter().zip(col_indices.iter()) {
                let target_col = &schema.columns[target_idx];
                let bound_expr = crate::sql::bind_expr(
                    expr,
                    &crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema),
                )?;
                let mut evaluated_val = bound_expr.evaluate(&empty_row, &schema)?;

                // Type coercion for numeric literals
                evaluated_val = match (evaluated_val, target_col.data_type) {
                    (Value::Int64(i), DataType::UInt64) if i >= 0 => Value::UInt64(i as u64),
                    (Value::Int64(i), DataType::Float64) => Value::Float64(i as f64),
                    (Value::UInt64(u), DataType::Int64) if u <= i64::MAX as u64 => {
                        Value::Int64(u as i64)
                    }
                    (Value::UInt64(u), DataType::Float64) => Value::Float64(u as f64),
                    (val, _) => val,
                };

                // Validate nullability and type compatibility
                if evaluated_val.is_null() {
                    if !target_col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: target_col.name.clone(),
                        });
                    }
                } else if evaluated_val.data_type() != target_col.data_type {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: evaluated_val.data_type().to_string(),
                    });
                }

                row_values[target_idx] = evaluated_val;
                populated[target_idx] = true;
            }

            // Check any omitted columns: must be nullable
            for (i, is_pop) in populated.iter().enumerate() {
                if !is_pop {
                    let col = &schema.columns[i];
                    if !col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: col.name.clone(),
                        });
                    }
                    row_values[i] = Value::Null;
                }
            }

            rows_to_insert.push(crate::engine::types::Row::new(row_values));
        }

        // 3. Perform inserts through Table handle
        let mut inserted_count = 0;
        for row in rows_to_insert {
            table.insert(&row)?;
            inserted_count += 1;
        }

        Ok(StatementResult::Inserted {
            rows: inserted_count,
        })
    }

    fn execute_update(&mut self, stmt: &crate::sql::UpdateStatement) -> Result<StatementResult> {
        self.ensure_active()?;
        let schema = {
            let table = self.open_table(&stmt.table_name)?;
            table.schema().clone()
        };

        // 1. Validate assignments
        let mut col_assignments: Vec<(usize, crate::query::expr::Expr)> =
            Vec::with_capacity(stmt.assignments.len());
        let mut assigned_cols = std::collections::HashSet::new();

        for assign in &stmt.assignments {
            if !assigned_cols.insert(&assign.column_name) {
                return Err(Error::TableSchemaMismatch(format!(
                    "duplicate assignment to column '{}' in UPDATE",
                    assign.column_name
                )));
            }

            let col_idx = schema
                .columns
                .iter()
                .position(|c| c.name == assign.column_name)
                .ok_or_else(|| Error::ColumnNotFound(assign.column_name.clone()))?;

            // Reject primary key column modification
            if col_idx == schema.primary_key_idx {
                return Err(Error::InvalidPrimaryKey(format!(
                    "cannot update primary key column '{}'",
                    assign.column_name
                )));
            }

            let bound_expr = crate::sql::bind_expr(
                &assign.expr,
                &crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema),
            )?;
            let expr_type = bound_expr.infer_type(&schema)?;
            let target_col = &schema.columns[col_idx];

            if expr_type != DataType::Null && expr_type != target_col.data_type {
                let can_coerce = matches!(
                    (&bound_expr, target_col.data_type),
                    (crate::query::expr::Expr::Literal(Value::Int64(i)), DataType::UInt64) if *i >= 0
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (
                        crate::query::expr::Expr::Literal(Value::Int64(_)),
                        DataType::Float64
                    )
                );
                if !can_coerce {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: expr_type.to_string(),
                    });
                }
            }

            col_assignments.push((col_idx, bound_expr));
        }

        // 2. Use optimizer to select access path and candidate rows
        let select_ast = crate::sql::SelectStatement {
            projection: vec![crate::sql::SelectItem::Wildcard],
            from: crate::sql::TableRef::new(&stmt.table_name),
            joins: Vec::new(),
            selection: stmt.selection.clone(),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let bound_select = crate::sql::bind_select_multi(
            &select_ast,
            &[crate::sql::TableBinding::new(
                &stmt.table_name,
                None,
                &schema,
            )],
        )?;

        let table_opt_info = {
            let table = self.open_table(&stmt.table_name)?;
            let indexes = table.indexes().iter().map(|(m, _)| m.clone()).collect();
            crate::query::optimizer::TableOptInfo::new(
                &stmt.table_name,
                None::<String>,
                schema.clone(),
                indexes,
            )
        };
        let plan = crate::query::optimizer::Optimizer::optimize(&bound_select, &[table_opt_info])?;

        // 3. Fetch candidate rows
        let candidate_rows = {
            let mut table = self.open_table(&stmt.table_name)?;
            let initial_rows = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                    match table.get(key)? {
                        Some(row) => vec![row],
                        None => Vec::new(),
                    }
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                    table.scan(start.as_ref(), end.as_ref())?
                }
                crate::query::optimizer::AccessPath::SecondaryIndexScan {
                    index_name,
                    scan_type,
                    ..
                } => match scan_type {
                    crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                        table.index_lookup(index_name, val)?
                    }
                    crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                    }
                },
                crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
            };

            match &plan.base_table.residual_filter {
                Some(filter) => crate::query::FilterOperator::apply(initial_rows, &schema, filter)?,
                None => initial_rows,
            }
        };

        // 4. Update each matching row
        let mut updated_count = 0;
        let mut table = self.open_table(&stmt.table_name)?;

        for old_row in candidate_rows {
            let mut new_values = old_row.values.clone();

            for (col_idx, expr) in &col_assignments {
                let target_col = &schema.columns[*col_idx];
                let mut val = expr.evaluate(&old_row, &schema)?;

                val = match (val, target_col.data_type) {
                    (Value::Int64(i), DataType::UInt64) if i >= 0 => Value::UInt64(i as u64),
                    (Value::Int64(i), DataType::Float64) => Value::Float64(i as f64),
                    (Value::UInt64(u), DataType::Int64) if u <= i64::MAX as u64 => {
                        Value::Int64(u as i64)
                    }
                    (Value::UInt64(u), DataType::Float64) => Value::Float64(u as f64),
                    (v, _) => v,
                };

                if val.is_null() {
                    if !target_col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: target_col.name.clone(),
                        });
                    }
                } else if val.data_type() != target_col.data_type {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: val.data_type().to_string(),
                    });
                }

                new_values[*col_idx] = val;
            }

            let new_row = crate::engine::types::Row::new(new_values);
            table.update(&new_row)?;
            updated_count += 1;
        }

        Ok(StatementResult::Updated {
            rows: updated_count,
        })
    }

    fn execute_delete(&mut self, stmt: &crate::sql::DeleteStatement) -> Result<StatementResult> {
        self.ensure_active()?;
        let schema = {
            let table = self.open_table(&stmt.table_name)?;
            table.schema().clone()
        };

        // 1. Use optimizer to select access path and candidate rows
        let select_ast = crate::sql::SelectStatement {
            projection: vec![crate::sql::SelectItem::Wildcard],
            from: crate::sql::TableRef::new(&stmt.table_name),
            joins: Vec::new(),
            selection: stmt.selection.clone(),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let bound_select = crate::sql::bind_select_multi(
            &select_ast,
            &[crate::sql::TableBinding::new(
                &stmt.table_name,
                None,
                &schema,
            )],
        )?;

        let table_opt_info = {
            let table = self.open_table(&stmt.table_name)?;
            let indexes = table.indexes().iter().map(|(m, _)| m.clone()).collect();
            crate::query::optimizer::TableOptInfo::new(
                &stmt.table_name,
                None::<String>,
                schema.clone(),
                indexes,
            )
        };
        let plan = crate::query::optimizer::Optimizer::optimize(&bound_select, &[table_opt_info])?;

        // 2. Fetch candidate rows
        let candidate_rows = {
            let mut table = self.open_table(&stmt.table_name)?;
            let initial_rows = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                    match table.get(key)? {
                        Some(row) => vec![row],
                        None => Vec::new(),
                    }
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                    table.scan(start.as_ref(), end.as_ref())?
                }
                crate::query::optimizer::AccessPath::SecondaryIndexScan {
                    index_name,
                    scan_type,
                    ..
                } => match scan_type {
                    crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                        table.index_lookup(index_name, val)?
                    }
                    crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                    }
                },
                crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
            };

            match &plan.base_table.residual_filter {
                Some(filter) => crate::query::FilterOperator::apply(initial_rows, &schema, filter)?,
                None => initial_rows,
            }
        };

        // 3. Delete matching rows by primary key
        let mut deleted_count = 0;
        let mut table = self.open_table(&stmt.table_name)?;

        for row in candidate_rows {
            let pk_val = &row.values[schema.primary_key_idx];
            if table.delete(pk_val)?.is_some() {
                deleted_count += 1;
            }
        }

        Ok(StatementResult::Deleted {
            rows: deleted_count,
        })
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction.
    pub fn execute_physical_plan(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;

        // 1. Execute base table scan
        let (mut current_schema, mut current_rows) = {
            let mut table = self.open_table(&plan.base_table.table_name)?;
            let raw_schema = table.schema().clone();
            let initial_rows = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                    match table.get(key)? {
                        Some(row) => vec![row],
                        None => Vec::new(),
                    }
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                    table.scan(start.as_ref(), end.as_ref())?
                }
                crate::query::optimizer::AccessPath::SecondaryIndexScan {
                    index_name,
                    scan_type,
                    ..
                } => match scan_type {
                    crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                        table.index_lookup(index_name, val)?
                    }
                    crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                    }
                },
                crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
            };

            let filtered_rows = match &plan.base_table.residual_filter {
                Some(filter) => {
                    crate::query::FilterOperator::apply(initial_rows, &raw_schema, filter)?
                }
                None => initial_rows,
            };

            let schema = if plan.joins.is_empty() {
                raw_schema
            } else {
                let q = plan.base_table.qualifier();
                let cols: Vec<crate::engine::types::Column> = raw_schema
                    .columns
                    .iter()
                    .map(|c| {
                        crate::engine::types::Column::new(
                            format!("{q}.{}", c.name),
                            c.data_type,
                            c.nullable,
                        )
                    })
                    .collect();
                crate::engine::types::Schema::new_projected(cols)
            };

            (schema, filtered_rows)
        };

        // 2. Execute Joins sequentially
        for join in &plan.joins {
            let (right_schema, right_rows) = {
                let mut table = self.open_table(&join.right.table_name)?;
                let raw_schema = table.schema().clone();
                let initial_rows = match &join.right.access_path {
                    crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                        match table.get(key)? {
                            Some(row) => vec![row],
                            None => Vec::new(),
                        }
                    }
                    crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                        table.scan(start.as_ref(), end.as_ref())?
                    }
                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                        index_name,
                        scan_type,
                        ..
                    } => match scan_type {
                        crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                            table.index_lookup(index_name, val)?
                        }
                        crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                            table.index_scan(index_name, start.as_ref(), end.as_ref())?
                        }
                    },
                    crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
                };

                let filtered_rows = match &join.right.residual_filter {
                    Some(filter) => {
                        crate::query::FilterOperator::apply(initial_rows, &raw_schema, filter)?
                    }
                    None => initial_rows,
                };

                let q = join.right.qualifier();
                let cols: Vec<crate::engine::types::Column> = raw_schema
                    .columns
                    .iter()
                    .map(|c| {
                        crate::engine::types::Column::new(
                            format!("{q}.{}", c.name),
                            c.data_type,
                            c.nullable,
                        )
                    })
                    .collect();
                (
                    crate::engine::types::Schema::new_projected(cols),
                    filtered_rows,
                )
            };

            let (next_schema, next_rows) = crate::query::JoinOperator::apply(
                current_rows,
                &current_schema,
                &right_rows,
                &right_schema,
                join.join_type,
                &join.on,
            )?;
            current_schema = next_schema;
            current_rows = next_rows;
        }

        // 3. Post-Join Filter
        if let Some(post_filter) = &plan.post_join_filter {
            current_rows =
                crate::query::FilterOperator::apply(current_rows, &current_schema, post_filter)?;
        }

        // 4. Aggregation / Group By / Having vs Sort / Project
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();
        if is_aggregate {
            let (grouped_schema, grouped_rows) = crate::query::AggregateOperator::apply(
                current_rows,
                &current_schema,
                &plan.group_by,
                &plan.aggregates,
            )?;

            let having_rows = match &plan.having {
                Some(predicate) => {
                    crate::query::FilterOperator::apply(grouped_rows, &grouped_schema, predicate)?
                }
                None => grouped_rows,
            };

            let sorted_rows =
                crate::query::SortOperator::apply(having_rows, &grouped_schema, &plan.order_by)?;

            let (projected_schema, projected_rows) = crate::query::ProjectOperator::apply_named(
                sorted_rows,
                &grouped_schema,
                &plan.projections,
            )?;
            let paginated_rows =
                crate::query::OffsetLimitOperator::apply(projected_rows, plan.offset, plan.limit);
            Ok(crate::query::QueryResult::new(
                projected_schema,
                paginated_rows,
            ))
        } else {
            let sorted_rows =
                crate::query::SortOperator::apply(current_rows, &current_schema, &plan.order_by)?;

            let (projected_schema, projected_rows) = crate::query::ProjectOperator::apply_named(
                sorted_rows,
                &current_schema,
                &plan.projections,
            )?;
            let paginated_rows =
                crate::query::OffsetLimitOperator::apply(projected_rows, plan.offset, plan.limit);
            Ok(crate::query::QueryResult::new(
                projected_schema,
                paginated_rows,
            ))
        }
    }

    /// Drops a table from the catalog and reclaims its pages within this transaction.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::drop_table(&mut self.db.buffer_pool, name)
    }

    /// Retrieves table metadata for `name` within this transaction.
    pub fn get_table(
        &mut self,
        name: &str,
    ) -> Result<Option<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, name)
    }

    /// Retrieves table metadata for `table_id` within this transaction.
    pub fn get_table_by_id(
        &mut self,
        table_id: crate::engine::catalog::TableId,
    ) -> Result<Option<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_table_by_id(&mut self.db.buffer_pool, table_id)
    }

    /// Lists all tables in the catalog within this transaction.
    pub fn list_tables(&mut self) -> Result<Vec<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::list_tables(&mut self.db.buffer_pool)
    }

    /// Validates the structural and referential integrity of the catalog within this transaction.
    pub fn validate_catalog(&mut self) -> Result<()> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::validate_catalog(&mut self.db.buffer_pool)
    }

    /// Executes a synchronous checkpoint during an active transaction.
    pub fn checkpoint(&mut self) -> Result<crate::wal::lsn::Lsn> {
        self.db.checkpoint()
    }

    /// Executes a synchronous checkpoint and truncates obsolete WAL records while preserving active history.
    pub fn checkpoint_and_truncate_wal(&mut self) -> Result<crate::wal::lsn::Lsn> {
        self.db.checkpoint_and_truncate_wal()
    }

    /// Validates the structural invariants of the B+ Tree.
    pub fn validate_tree(&mut self) -> Result<()> {
        self.db.validate_tree()
    }

    /// Returns a mutable reference to the underlying `BufferPool`.
    pub fn buffer_pool_mut(&mut self) -> &mut crate::storage::buffer_pool::BufferPool {
        self.db.buffer_pool_mut()
    }

    /// Commits all mutations performed in this transaction, flushing the WAL through the commit LSN.
    pub fn commit(mut self) -> Result<()> {
        self.ensure_active()?;
        self.db.commit_txn(self.txn_id)?;
        self.state = TxnState::Committed;
        Ok(())
    }

    /// Aborts all mutations performed in this transaction, rolling back physical page changes
    /// with Compensation Log Records (CLRs) and restoring pre-transaction database state.
    pub fn abort(mut self) -> Result<()> {
        self.ensure_active()?;
        self.abort_internal()?;
        Ok(())
    }

    fn ensure_active(&self) -> Result<()> {
        match self.state {
            TxnState::Active => Ok(()),
            TxnState::Committed => Err(Error::TransactionAlreadyCommitted(self.txn_id)),
            TxnState::Aborted => Err(Error::TransactionAlreadyAborted(self.txn_id)),
        }
    }

    fn abort_internal(&mut self) -> Result<()> {
        if self.state == TxnState::Active {
            self.db.abort_txn(self.txn_id)?;
            // Restore initial header metadata and tree root
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .root_page_id = self.initial_root_page_id;
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .freelist_head = self.initial_freelist_head;
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .free_page_count = self.initial_free_page_count;
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .page_count = self.initial_page_count;
            let _ = self
                .db
                .buffer_pool
                .page_manager_mut()
                .write_header_to_disk();
            self.db.tree =
                crate::index::bplus_tree::BPlusTree::from_root(self.initial_root_page_id);
            self.state = TxnState::Aborted;
        }
        Ok(())
    }
}

impl<'db> Drop for Transaction<'db> {
    fn drop(&mut self) {
        if self.state == TxnState::Active {
            let _ = self.abort_internal();
        }
    }
}
