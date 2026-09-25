use crate::engine::types::{DataType, Row, Schema, Value};
use crate::engine::TreeDb;
use crate::error::{Error, Result};
use crate::storage::page::{Key, PageId};
use crate::wal::lsn::TxnId;

/// Default maximum allowed recursive CTE iterations before aborting with an error.
pub const DEFAULT_MAX_RECURSION_ITERATIONS: usize = 1000;

/// Default maximum allowed recursive CTE total generated rows before aborting with an error.
pub const DEFAULT_MAX_RECURSION_ROWS: usize = 1_000_000;

fn ast_references_table(stmt: &crate::sql::SelectStatement, target_table: &str) -> bool {
    if stmt.from.name == target_table || stmt.from.alias.as_deref() == Some(target_table) {
        return true;
    }
    for join in &stmt.joins {
        if join.table.name == target_table || join.table.alias.as_deref() == Some(target_table) {
            return true;
        }
        if ast_expr_references_table(&join.on, target_table) {
            return true;
        }
    }
    if let Some(w) = &stmt.selection {
        if ast_expr_references_table(w, target_table) {
            return true;
        }
    }
    if let Some(h) = &stmt.having {
        if ast_expr_references_table(h, target_table) {
            return true;
        }
    }
    for item in &stmt.projection {
        match item {
            crate::sql::SelectItem::Expression { expr, .. } => {
                if ast_expr_references_table(expr, target_table) {
                    return true;
                }
            }
            crate::sql::SelectItem::QualifiedWildcard(tbl) => {
                if tbl == target_table {
                    return true;
                }
            }
            crate::sql::SelectItem::Wildcard => {}
        }
    }
    for item in &stmt.group_by {
        if ast_expr_references_table(item, target_table) {
            return true;
        }
    }
    for item in &stmt.order_by {
        if ast_expr_references_table(&item.expr, target_table) {
            return true;
        }
    }
    false
}

fn ast_expr_references_table(expr: &crate::sql::SqlExpr, target_table: &str) -> bool {
    match expr {
        crate::sql::SqlExpr::QualifiedIdentifier { qualifier, .. } => qualifier == target_table,
        crate::sql::SqlExpr::Identifier(_)
        | crate::sql::SqlExpr::Literal(_)
        | crate::sql::SqlExpr::CountAll => false,
        crate::sql::SqlExpr::Subquery(sub) => ast_references_table(sub, target_table),
        crate::sql::SqlExpr::Binary { left, right, .. } => {
            ast_expr_references_table(left, target_table)
                || ast_expr_references_table(right, target_table)
        }
        crate::sql::SqlExpr::Unary { expr, .. }
        | crate::sql::SqlExpr::IsNull(expr)
        | crate::sql::SqlExpr::IsNotNull(expr) => ast_expr_references_table(expr, target_table),
        crate::sql::SqlExpr::InList { expr, list, .. } => {
            ast_expr_references_table(expr, target_table)
                || list
                    .iter()
                    .any(|e| ast_expr_references_table(e, target_table))
        }
        crate::sql::SqlExpr::InSubquery { expr, subquery, .. } => {
            ast_expr_references_table(expr, target_table)
                || ast_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::Exists { subquery, .. } => {
            ast_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::Aggregate { arg, .. } => ast_expr_references_table(arg, target_table),
        crate::sql::SqlExpr::Window {
            args, window_spec, ..
        } => {
            args.iter()
                .any(|e| ast_expr_references_table(e, target_table))
                || window_spec
                    .partition_by
                    .iter()
                    .any(|e| ast_expr_references_table(e, target_table))
                || window_spec
                    .order_by
                    .iter()
                    .any(|o| ast_expr_references_table(&o.expr, target_table))
        }
    }
}

fn plan_references_table(plan: &crate::query::optimizer::PhysicalPlan, target_table: &str) -> bool {
    if plan.base_table.table_name == target_table
        || plan.base_table.table_alias.as_deref() == Some(target_table)
    {
        return true;
    }
    for join in &plan.joins {
        if join.right.table_name == target_table
            || join.right.table_alias.as_deref() == Some(target_table)
        {
            return true;
        }
    }
    false
}

fn validate_ast_cte_acyclicity(ctes: &[crate::sql::Cte]) -> Result<()> {
    let mut adj: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut is_rec = std::collections::HashMap::new();

    for cte in ctes {
        let mut deps = Vec::new();
        for other in ctes {
            if other.name == cte.name {
                continue;
            }
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, &other.name)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, &other.name)
                        || ast_references_table(recursive, &other.name)
                }
            };
            if refs {
                deps.push(other.name.as_str());
            }
        }
        adj.insert(cte.name.as_str(), deps);
        is_rec.insert(
            cte.name.as_str(),
            matches!(cte.query, crate::sql::CteQuery::Recursive { .. }),
        );
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Unvisited,
        Visiting,
        Visited,
    }

    let mut state = std::collections::HashMap::new();
    for cte in ctes {
        state.insert(cte.name.as_str(), VisitState::Unvisited);
    }

    fn dfs<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        is_rec: &std::collections::HashMap<&'a str, bool>,
        state: &mut std::collections::HashMap<&'a str, VisitState>,
        path: &mut Vec<&'a str>,
    ) -> Result<()> {
        state.insert(node, VisitState::Visiting);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for &neighbor in neighbors {
                match state
                    .get(neighbor)
                    .copied()
                    .unwrap_or(VisitState::Unvisited)
                {
                    VisitState::Visiting => {
                        path.push(neighbor);
                        let cycle_repr = path.join(" -> ");
                        let has_rec = path.iter().any(|n| is_rec.get(n).copied().unwrap_or(false));
                        if has_rec {
                            return Err(Error::MutualRecursion(cycle_repr));
                        }
                        return Err(Error::CyclicCteDependency(cycle_repr));
                    }
                    VisitState::Unvisited => {
                        dfs(neighbor, adj, is_rec, state, path)?;
                    }
                    VisitState::Visited => {}
                }
            }
        }

        path.pop();
        state.insert(node, VisitState::Visited);
        Ok(())
    }

    let mut path = Vec::new();
    for cte in ctes {
        if state.get(cte.name.as_str()).copied() == Some(VisitState::Unvisited) {
            dfs(cte.name.as_str(), &adj, &is_rec, &mut state, &mut path)?;
        }
    }

    Ok(())
}

struct TxnBufferPoolCatalog<'a, 'b> {
    bp: &'a std::cell::RefCell<&'b mut crate::storage::buffer_pool::BufferPool>,
    ctes: Option<&'a [crate::sql::BoundCte]>,
}

impl<'a, 'b> crate::sql::TableCatalog for TxnBufferPoolCatalog<'a, 'b> {
    fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        if let Some(ctes) = self.ctes {
            for cte in ctes {
                if cte.name == table_name {
                    return Ok(cte.output_schema.clone());
                }
            }
        }
        let mut bp = self.bp.borrow_mut();
        let meta = crate::engine::catalog::Catalog::get_table(&mut bp, table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        Ok(meta.schema)
    }

    fn get_table_indexes(
        &self,
        table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        if let Some(ctes) = self.ctes {
            for cte in ctes {
                if cte.name == table_name {
                    return Ok(Vec::new());
                }
            }
        }
        let mut bp = self.bp.borrow_mut();
        let meta = crate::engine::catalog::Catalog::get_table(&mut bp, table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        crate::engine::catalog::Catalog::list_indexes_for_table(&mut bp, meta.table_id)
    }
}

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
        self.plan_select_with_outer_ctes(ast, &[])
    }

    /// Plans a parsed `SelectStatement` AST with in-scope outer CTE definitions.
    pub fn plan_select_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let mut visible_ctes: Vec<crate::sql::BoundCte> = outer_ctes.to_vec();

        if let Some(with_clause) = &ast.with {
            validate_ast_cte_acyclicity(&with_clause.ctes)?;
            let mut declared_names = std::collections::HashSet::new();
            for cte in &with_clause.ctes {
                if !declared_names.insert(&cte.name) {
                    return Err(Error::DuplicateCteName(cte.name.clone()));
                }

                match &cte.query {
                    crate::sql::CteQuery::NonRecursive(select_stmt) => {
                        // Bind and plan the CTE query using currently visible CTEs
                        let cte_plan =
                            self.plan_select_with_outer_ctes(select_stmt, &visible_ctes)?;

                        // Derive the CTE output schema from cte_plan projections
                        let projected_schema =
                            self.derive_cte_output_schema(&cte_plan, &visible_ctes)?;

                        // If explicit column list was provided, validate and rename columns
                        let final_schema = if let Some(cols) = &cte.columns {
                            if cols.len() != projected_schema.columns.len() {
                                return Err(Error::CteColumnCountMismatch {
                                    cte: cte.name.clone(),
                                    expected: cols.len(),
                                    found: projected_schema.columns.len(),
                                });
                            }
                            let mut seen = std::collections::HashSet::new();
                            let mut renamed_cols = Vec::with_capacity(cols.len());
                            for (col_name, orig_col) in
                                cols.iter().zip(projected_schema.columns.iter())
                            {
                                if !seen.insert(col_name) {
                                    return Err(Error::DuplicateCteColumnName {
                                        cte: cte.name.clone(),
                                        column: col_name.clone(),
                                    });
                                }
                                renamed_cols.push(crate::engine::types::Column::new(
                                    col_name,
                                    orig_col.data_type,
                                    orig_col.nullable,
                                ));
                            }
                            crate::engine::types::Schema::new_projected(renamed_cols)
                        } else {
                            projected_schema
                        };

                        // Track dependencies
                        let mut dependencies = Vec::new();
                        collect_cte_dependencies(&cte_plan, &visible_ctes, &mut dependencies);

                        // Check for cyclic dependencies in visible_ctes + this CTE
                        let mut temp_ctes = visible_ctes.clone();
                        temp_ctes.push(crate::sql::BoundCte {
                            name: cte.name.clone(),
                            output_schema: final_schema.clone(),
                            kind: crate::sql::BoundCteKind::NonRecursive {
                                plan: cte_plan.clone(),
                            },
                            dependencies: dependencies.clone(),
                        });
                        validate_cte_acyclicity(&temp_ctes)?;

                        visible_ctes.push(crate::sql::BoundCte {
                            name: cte.name.clone(),
                            output_schema: final_schema,
                            kind: crate::sql::BoundCteKind::NonRecursive { plan: cte_plan },
                            dependencies,
                        });
                    }
                    crate::sql::CteQuery::Recursive { anchor, recursive } => {
                        // 1. Check anchor does not reference cte.name
                        if ast_references_table(anchor, &cte.name) {
                            return Err(Error::RecursiveCteAnchorSelfReference(cte.name.clone()));
                        }

                        // 2. Bind and plan anchor
                        let anchor_plan =
                            self.plan_select_with_outer_ctes(anchor, &visible_ctes)?;
                        let projected_schema =
                            self.derive_cte_output_schema(&anchor_plan, &visible_ctes)?;

                        // 3. Rename columns if explicit columns given
                        let final_schema = if let Some(cols) = &cte.columns {
                            if cols.len() != projected_schema.columns.len() {
                                return Err(Error::CteColumnCountMismatch {
                                    cte: cte.name.clone(),
                                    expected: cols.len(),
                                    found: projected_schema.columns.len(),
                                });
                            }
                            let mut seen = std::collections::HashSet::new();
                            let mut renamed_cols = Vec::with_capacity(cols.len());
                            for (col_name, orig_col) in
                                cols.iter().zip(projected_schema.columns.iter())
                            {
                                if !seen.insert(col_name) {
                                    return Err(Error::DuplicateCteColumnName {
                                        cte: cte.name.clone(),
                                        column: col_name.clone(),
                                    });
                                }
                                renamed_cols.push(crate::engine::types::Column::new(
                                    col_name,
                                    orig_col.data_type,
                                    orig_col.nullable,
                                ));
                            }
                            crate::engine::types::Schema::new_projected(renamed_cols)
                        } else {
                            projected_schema
                        };

                        // 4. Create scope where cte.name is available with final_schema
                        let mut rec_visible_ctes = visible_ctes.clone();
                        rec_visible_ctes.push(crate::sql::BoundCte {
                            name: cte.name.clone(),
                            output_schema: final_schema.clone(),
                            kind: crate::sql::BoundCteKind::NonRecursive {
                                plan: anchor_plan.clone(),
                            },
                            dependencies: Vec::new(),
                        });

                        // 5. Plan recursive member
                        let rec_plan =
                            self.plan_select_with_outer_ctes(recursive, &rec_visible_ctes)?;

                        // 6. Check unsupported recursive aggregation or window functions
                        let self_ref_in_rec = plan_references_table(&rec_plan, &cte.name);
                        if self_ref_in_rec {
                            if !rec_plan.group_by.is_empty() || !rec_plan.aggregates.is_empty() {
                                return Err(Error::UnsupportedRecursiveAggregation(
                                    cte.name.clone(),
                                ));
                            }
                            if !rec_plan.window_exprs.is_empty() {
                                return Err(Error::UnsupportedRecursiveWindow(cte.name.clone()));
                            }
                        }

                        // 7. Derive recursive member output schema
                        let rec_projected_schema =
                            self.derive_cte_output_schema(&rec_plan, &rec_visible_ctes)?;

                        // 8. Validate schema compatibility between anchor and recursive member
                        if final_schema.columns.len() != rec_projected_schema.columns.len() {
                            return Err(Error::RecursiveCteColumnCountMismatch {
                                cte: cte.name.clone(),
                                anchor_cols: final_schema.columns.len(),
                                rec_cols: rec_projected_schema.columns.len(),
                            });
                        }
                        for (a_col, r_col) in final_schema
                            .columns
                            .iter()
                            .zip(rec_projected_schema.columns.iter())
                        {
                            if a_col.data_type != r_col.data_type {
                                return Err(Error::RecursiveCteSchemaMismatch {
                                    cte: cte.name.clone(),
                                    anchor_col: a_col.name.clone(),
                                    anchor_type: format!("{:?}", a_col.data_type),
                                    rec_col: r_col.name.clone(),
                                    rec_type: format!("{:?}", r_col.data_type),
                                });
                            }
                        }

                        // 9. Collect dependencies on earlier CTEs (from both anchor and rec member, excluding cte.name)
                        let mut dependencies = Vec::new();
                        collect_cte_dependencies(&anchor_plan, &visible_ctes, &mut dependencies);
                        collect_cte_dependencies(&rec_plan, &visible_ctes, &mut dependencies);

                        let mut temp_ctes = visible_ctes.clone();
                        temp_ctes.push(crate::sql::BoundCte {
                            name: cte.name.clone(),
                            output_schema: final_schema.clone(),
                            kind: crate::sql::BoundCteKind::Recursive {
                                anchor_plan: anchor_plan.clone(),
                                recursive_plan: rec_plan.clone(),
                                max_iterations: DEFAULT_MAX_RECURSION_ITERATIONS,
                                max_rows: DEFAULT_MAX_RECURSION_ROWS,
                            },
                            dependencies: dependencies.clone(),
                        });
                        validate_cte_acyclicity(&temp_ctes)?;

                        visible_ctes.push(crate::sql::BoundCte {
                            name: cte.name.clone(),
                            output_schema: final_schema,
                            kind: crate::sql::BoundCteKind::Recursive {
                                anchor_plan,
                                recursive_plan: rec_plan,
                                max_iterations: DEFAULT_MAX_RECURSION_ITERATIONS,
                                max_rows: DEFAULT_MAX_RECURSION_ROWS,
                            },
                            dependencies,
                        });
                    }
                }
            }
        }

        // Now plan the main query body with visible_ctes
        let mut table_infos = Vec::new();
        let mut bindings = Vec::new();

        // 1. Resolve FROM table
        {
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(&ast.from.name, &visible_ctes)?;
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(
                    &ast.from.name,
                    ast.from.alias.as_deref(),
                    schema,
                    indexes,
                )
                .with_cte(is_cte),
            );
        }

        // 2. Resolve JOIN tables
        for j in &ast.joins {
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(&j.table.name, &visible_ctes)?;
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(
                    &j.table.name,
                    j.table.alias.as_deref(),
                    schema,
                    indexes,
                )
                .with_cte(is_cte),
            );
        }

        for t in &table_infos {
            bindings.push(crate::sql::TableBinding::new(
                &t.name,
                t.alias.as_deref(),
                &t.schema,
            ));
        }

        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
        let catalog = TxnBufferPoolCatalog {
            bp: &bp_cell,
            ctes: Some(&visible_ctes),
        };
        let mut bound = crate::sql::bind_select_multi_with_catalog(ast, &bindings, Some(&catalog))?;
        bound.ctes = visible_ctes;
        crate::query::optimizer::Optimizer::optimize(&bound, &table_infos)
    }

    fn resolve_table_for_planning(
        &mut self,
        table_name: &str,
        visible_ctes: &[crate::sql::BoundCte],
    ) -> Result<(
        crate::engine::types::Schema,
        Vec<crate::engine::catalog::IndexMetadata>,
        bool,
    )> {
        // CTEs shadow physical tables
        for cte in visible_ctes {
            if cte.name == table_name {
                return Ok((cte.output_schema.clone(), Vec::new(), true));
            }
        }
        let table = self.open_table(table_name)?;
        let schema = table.schema().clone();
        let indexes = table.indexes().iter().map(|(m, _)| m.clone()).collect();
        Ok((schema, indexes, false))
    }

    fn derive_cte_output_schema(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        visible_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::engine::types::Schema> {
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();

        let (base_schema, _, _) =
            self.resolve_table_for_planning(&plan.base_table.table_name, visible_ctes)?;

        let current_schema = if plan.joins.is_empty() {
            base_schema
        } else {
            let mut cols = Vec::new();
            let q = plan.base_table.qualifier();
            for col in &base_schema.columns {
                cols.push(crate::engine::types::Column::new(
                    format!("{q}.{}", col.name),
                    col.data_type,
                    col.nullable,
                ));
            }
            for join in &plan.joins {
                let (right_schema, _, _) =
                    self.resolve_table_for_planning(&join.right.table_name, visible_ctes)?;
                let rq = join.right.qualifier();
                for col in &right_schema.columns {
                    let mut right_col = col.clone();
                    if join.join_type == crate::sql::ast::JoinType::Left {
                        right_col.nullable = true;
                    }
                    cols.push(crate::engine::types::Column::new(
                        format!("{rq}.{}", right_col.name),
                        right_col.data_type,
                        right_col.nullable,
                    ));
                }
            }
            crate::engine::types::Schema::new_projected(cols)
        };

        let intermediate_schema = if is_aggregate {
            let mut grouped_cols = Vec::new();
            for gb in &plan.group_by {
                let dt = gb.infer_type(&current_schema)?;
                let name = match gb {
                    crate::query::expr::Expr::Column(n) => n.clone(),
                    _ => format!("{gb}"),
                };
                grouped_cols.push(crate::engine::types::Column::new(name, dt, true));
            }
            for agg in &plan.aggregates {
                let dt = agg.infer_type(&current_schema)?;
                grouped_cols.push(crate::engine::types::Column::new(
                    agg.display_name(),
                    dt,
                    true,
                ));
            }
            crate::engine::types::Schema::new_projected(grouped_cols)
        } else {
            current_schema
        };

        let intermediate_schema = if !plan.window_exprs.is_empty() {
            let mut w_cols = intermediate_schema.columns.clone();
            for we in &plan.window_exprs {
                w_cols.push(crate::engine::types::Column::new(
                    we.display_name.clone(),
                    we.output_type,
                    true,
                ));
            }
            crate::engine::types::Schema::new_projected(w_cols)
        } else {
            intermediate_schema
        };

        let mut out_cols = Vec::with_capacity(plan.projections.len());
        for (idx, (expr, alias)) in plan.projections.iter().enumerate() {
            let dt = expr.infer_type(&intermediate_schema)?;
            let name = match alias {
                Some(alias_name) => alias_name.clone(),
                None => match expr {
                    crate::query::expr::Expr::Column(col_name) => {
                        if let Some(dot_idx) = col_name.find('.') {
                            col_name[dot_idx + 1..].to_string()
                        } else {
                            col_name.clone()
                        }
                    }
                    _ => format!("col_{idx}"),
                },
            };
            let nullable = match expr {
                crate::query::expr::Expr::Column(col_name) => intermediate_schema
                    .get_column(col_name)
                    .map(|c| c.nullable)
                    .unwrap_or(true),
                crate::query::expr::Expr::Literal(Value::Null) => true,
                _ => true,
            };
            out_cols.push(crate::engine::types::Column::new(name, dt, nullable));
        }

        Ok(crate::engine::types::Schema::new_projected(out_cols))
    }
}

fn collect_cte_dependencies(
    plan: &crate::query::optimizer::PhysicalPlan,
    visible_ctes: &[crate::sql::BoundCte],
    deps: &mut Vec<String>,
) {
    let mut check_table = |t_name: &str| {
        for v in visible_ctes {
            if v.name == t_name && !deps.contains(&v.name) {
                deps.push(v.name.clone());
            }
        }
    };

    check_table(&plan.base_table.table_name);
    for j in &plan.joins {
        check_table(&j.right.table_name);
    }
    if let Some(rf) = &plan.base_table.residual_filter {
        collect_cte_deps_from_expr(rf, visible_ctes, deps);
    }
    for j in &plan.joins {
        collect_cte_deps_from_expr(&j.on, visible_ctes, deps);
        if let Some(rf) = &j.right.residual_filter {
            collect_cte_deps_from_expr(rf, visible_ctes, deps);
        }
    }
    if let Some(post) = &plan.post_join_filter {
        collect_cte_deps_from_expr(post, visible_ctes, deps);
    }
    if let Some(h) = &plan.having {
        collect_cte_deps_from_expr(h, visible_ctes, deps);
    }
    for (p, _) in &plan.projections {
        collect_cte_deps_from_expr(p, visible_ctes, deps);
    }
    for o in &plan.order_by {
        collect_cte_deps_from_expr(&o.expression, visible_ctes, deps);
    }
}

fn collect_cte_deps_from_expr(
    expr: &crate::query::expr::Expr,
    visible_ctes: &[crate::sql::BoundCte],
    deps: &mut Vec<String>,
) {
    match expr {
        crate::query::expr::Expr::Subquery(sub) => {
            collect_cte_dependencies(&sub.plan, visible_ctes, deps);
        }
        crate::query::expr::Expr::Binary { left, right, .. } => {
            collect_cte_deps_from_expr(left, visible_ctes, deps);
            collect_cte_deps_from_expr(right, visible_ctes, deps);
        }
        crate::query::expr::Expr::Unary { expr, .. }
        | crate::query::expr::Expr::IsNull(expr)
        | crate::query::expr::Expr::IsNotNull(expr) => {
            collect_cte_deps_from_expr(expr, visible_ctes, deps);
        }
        crate::query::expr::Expr::InList { expr, list, .. } => {
            collect_cte_deps_from_expr(expr, visible_ctes, deps);
            for item in list {
                collect_cte_deps_from_expr(item, visible_ctes, deps);
            }
        }
        crate::query::expr::Expr::Literal(_)
        | crate::query::expr::Expr::Column(_)
        | crate::query::expr::Expr::OuterColumn { .. } => {}
    }
}

fn validate_cte_acyclicity(ctes: &[crate::sql::BoundCte]) -> Result<()> {
    let mut adj = std::collections::HashMap::new();
    for cte in ctes {
        adj.insert(cte.name.as_str(), &cte.dependencies);
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Unvisited,
        Visiting,
        Visited,
    }

    let mut state = std::collections::HashMap::new();
    for cte in ctes {
        state.insert(cte.name.as_str(), VisitState::Unvisited);
    }

    fn dfs<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, &'a Vec<String>>,
        state: &mut std::collections::HashMap<&'a str, VisitState>,
        path: &mut Vec<&'a str>,
        ctes: &[crate::sql::BoundCte],
    ) -> Result<()> {
        state.insert(node, VisitState::Visiting);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for neighbor in *neighbors {
                let n_str = neighbor.as_str();
                match state.get(n_str).copied().unwrap_or(VisitState::Unvisited) {
                    VisitState::Visiting => {
                        path.push(n_str);
                        let cycle_repr = path.join(" -> ");
                        if ctes.iter().any(|c| {
                            (c.name == n_str || path.contains(&c.name.as_str())) && c.is_recursive()
                        }) {
                            return Err(Error::MutualRecursion(cycle_repr));
                        }
                        return Err(Error::CyclicCteDependency(cycle_repr));
                    }
                    VisitState::Unvisited => {
                        dfs(n_str, adj, state, path, ctes)?;
                    }
                    VisitState::Visited => {}
                }
            }
        }

        path.pop();
        state.insert(node, VisitState::Visited);
        Ok(())
    }

    let mut path = Vec::new();
    for cte in ctes {
        if state.get(cte.name.as_str()).copied() == Some(VisitState::Unvisited) {
            dfs(cte.name.as_str(), &adj, &mut state, &mut path, ctes)?;
        }
    }

    Ok(())
}

impl<'db> Transaction<'db> {
    /// Parses, binds, and produces an optimized physical execution plan (`PhysicalPlan`) for the given SQL query or DML filter.
    pub fn plan_sql(&mut self, sql: &str) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => self.plan_select(&select_stmt),
            crate::sql::Statement::Update(update_stmt) => {
                let select_ast = crate::sql::SelectStatement {
                    with: None,
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
                    with: None,
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
            with: None,
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
        let mut plan = self.plan_select(&select_ast)?;

        // 3. Fetch candidate rows
        self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
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
                Some(filter) => self.filter_rows_with_outer(initial_rows, &schema, filter, &[])?,
                None => initial_rows,
            }
        };

        // 4. Update each matching row
        let mut updated_rows = Vec::new();
        for old_row in candidate_rows {
            let mut new_values = old_row.values.clone();

            for (col_idx, expr) in &col_assignments {
                let target_col = &schema.columns[*col_idx];
                let mut val = self.evaluate_expr_with_outer(expr, &old_row, &schema, &[])?;

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

            updated_rows.push(crate::engine::types::Row::new(new_values));
        }

        let mut updated_count = 0;
        let mut table = self.open_table(&stmt.table_name)?;
        for new_row in updated_rows {
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
            with: None,
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
        let mut plan = self.plan_select(&select_ast)?;

        // 2. Fetch candidate rows
        self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
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
                Some(filter) => self.filter_rows_with_outer(initial_rows, &schema, filter, &[])?,
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
        self.execute_physical_plan_with_outer(plan, &[])
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction with outer scope contexts.
    pub fn execute_physical_plan_with_outer(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<crate::query::QueryResult> {
        let mut cte_store = std::collections::HashMap::new();
        self.execute_physical_plan_internal(plan, outer_contexts, &mut cte_store)
    }

    fn execute_physical_plan_internal(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;

        // 0. Materialize any CTEs defined on this plan (in declaration order)
        for cte in &plan.ctes {
            if !cte_store.contains_key(&cte.name) {
                match &cte.kind {
                    crate::sql::BoundCteKind::NonRecursive { plan } => {
                        let res =
                            self.execute_physical_plan_internal(plan, outer_contexts, cte_store)?;
                        cte_store.insert(cte.name.clone(), (cte.output_schema.clone(), res.rows));
                    }
                    crate::sql::BoundCteKind::Recursive {
                        anchor_plan,
                        recursive_plan,
                        max_iterations,
                        max_rows,
                    } => {
                        let anchor_res = self.execute_physical_plan_internal(
                            anchor_plan,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut accumulated_rows = anchor_res.rows;
                        if accumulated_rows.len() > *max_rows {
                            return Err(Error::RecursiveCteRowLimit {
                                cte: cte.name.clone(),
                                rows: accumulated_rows.len(),
                                limit: *max_rows,
                            });
                        }
                        let mut working_rows = accumulated_rows.clone();
                        let mut iterations = 0;

                        while !working_rows.is_empty() {
                            iterations += 1;
                            if iterations > *max_iterations {
                                return Err(Error::RecursiveCteIterationLimit {
                                    cte: cte.name.clone(),
                                    iterations,
                                    limit: *max_iterations,
                                });
                            }

                            cte_store.insert(
                                cte.name.clone(),
                                (cte.output_schema.clone(), working_rows),
                            );

                            let next_res = self.execute_physical_plan_internal(
                                recursive_plan,
                                outer_contexts,
                                cte_store,
                            )?;
                            let next_working_rows = next_res.rows;

                            if next_working_rows.is_empty() {
                                break;
                            }

                            if accumulated_rows.len() + next_working_rows.len() > *max_rows {
                                return Err(Error::RecursiveCteRowLimit {
                                    cte: cte.name.clone(),
                                    rows: accumulated_rows.len() + next_working_rows.len(),
                                    limit: *max_rows,
                                });
                            }

                            accumulated_rows.extend(next_working_rows.clone());
                            working_rows = next_working_rows;
                        }

                        cte_store.insert(
                            cte.name.clone(),
                            (cte.output_schema.clone(), accumulated_rows),
                        );
                    }
                }
            }
        }

        let mut plan = plan.clone();
        self.pre_evaluate_uncorrelated_subqueries_internal(&mut plan, outer_contexts, cte_store)?;

        // 1. Execute base table scan
        let (mut current_schema, mut current_rows) = {
            if let Some((cte_schema, cte_rows)) = cte_store.get(&plan.base_table.table_name) {
                let raw_schema = cte_schema.clone();
                let initial_rows = cte_rows.clone();
                let filtered_rows = match &plan.base_table.residual_filter {
                    Some(filter) => self.filter_rows_with_outer_internal(
                        initial_rows,
                        &raw_schema,
                        filter,
                        outer_contexts,
                        cte_store,
                    )?,
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
            } else {
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
                    Some(filter) => self.filter_rows_with_outer_internal(
                        initial_rows,
                        &raw_schema,
                        filter,
                        outer_contexts,
                        cte_store,
                    )?,
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
            }
        };

        // 2. Execute Joins sequentially
        for join in &plan.joins {
            let (right_schema, right_rows) = {
                if let Some((cte_schema, cte_rows)) = cte_store.get(&join.right.table_name) {
                    let raw_schema = cte_schema.clone();
                    let initial_rows = cte_rows.clone();
                    let filtered_rows = match &join.right.residual_filter {
                        Some(filter) => self.filter_rows_with_outer_internal(
                            initial_rows,
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?,
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
                } else {
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
                            crate::query::optimizer::SecondaryScanType::RangeScan {
                                start,
                                end,
                            } => table.index_scan(index_name, start.as_ref(), end.as_ref())?,
                        },
                        crate::query::optimizer::AccessPath::FullTableScan => {
                            table.scan(None, None)?
                        }
                    };

                    let filtered_rows = match &join.right.residual_filter {
                        Some(filter) => self.filter_rows_with_outer_internal(
                            initial_rows,
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?,
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
                }
            };

            let (next_schema, next_rows) = self.join_rows_with_outer_internal(
                current_rows,
                &current_schema,
                &right_rows,
                &right_schema,
                join.join_type,
                &join.on,
                outer_contexts,
                cte_store,
            )?;
            current_schema = next_schema;
            current_rows = next_rows;
        }

        // 3. Post-Join Filter
        if let Some(post_filter) = &plan.post_join_filter {
            current_rows = self.filter_rows_with_outer_internal(
                current_rows,
                &current_schema,
                post_filter,
                outer_contexts,
                cte_store,
            )?;
        }

        // 4. Aggregation / Group By / Having
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();
        let (mut working_schema, mut working_rows) = if is_aggregate {
            let (grouped_schema, grouped_rows) = crate::query::AggregateOperator::apply(
                current_rows,
                &current_schema,
                &plan.group_by,
                &plan.aggregates,
            )?;

            let having_rows = match &plan.having {
                Some(predicate) => self.filter_rows_with_outer_internal(
                    grouped_rows,
                    &grouped_schema,
                    predicate,
                    outer_contexts,
                    cte_store,
                )?,
                None => grouped_rows,
            };
            (grouped_schema, having_rows)
        } else {
            (current_schema, current_rows)
        };

        // 5. Window Operator
        if !plan.window_exprs.is_empty() {
            let (win_schema, win_rows) = crate::query::WindowOperator::apply(
                working_rows,
                &working_schema,
                &plan.window_exprs,
            )?;
            working_schema = win_schema;
            working_rows = win_rows;
        }

        // 6. Sort Operator (ORDER BY)
        let sorted_rows =
            crate::query::SortOperator::apply(working_rows, &working_schema, &plan.order_by)?;

        // 7. Projection
        let (projected_schema, projected_rows) = crate::query::ProjectOperator::apply_named(
            sorted_rows,
            &working_schema,
            &plan.projections,
        )?;

        // 8. Pagination (OFFSET / LIMIT)
        let paginated_rows =
            crate::query::OffsetLimitOperator::apply(projected_rows, plan.offset, plan.limit);
        Ok(crate::query::QueryResult::new(
            projected_schema,
            paginated_rows,
        ))
    }

    /// Pre-evaluates all uncorrelated subqueries in the physical plan.
    pub fn pre_evaluate_uncorrelated_subqueries(
        &mut self,
        plan: &mut crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<()> {
        let mut cte_store = std::collections::HashMap::new();
        self.pre_evaluate_uncorrelated_subqueries_internal(plan, outer_contexts, &mut cte_store)
    }

    fn pre_evaluate_uncorrelated_subqueries_internal(
        &mut self,
        plan: &mut crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<()> {
        if let Some(rf) = &mut plan.base_table.residual_filter {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(rf, outer_contexts, cte_store)?;
        }
        for join in &mut plan.joins {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(
                &mut join.on,
                outer_contexts,
                cte_store,
            )?;
            if let Some(rf) = &mut join.right.residual_filter {
                self.pre_evaluate_uncorrelated_subqueries_in_expr(rf, outer_contexts, cte_store)?;
            }
        }
        if let Some(post_filter) = &mut plan.post_join_filter {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(
                post_filter,
                outer_contexts,
                cte_store,
            )?;
        }
        if let Some(having) = &mut plan.having {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(having, outer_contexts, cte_store)?;
        }
        for (proj, _) in &mut plan.projections {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(proj, outer_contexts, cte_store)?;
        }
        for sort_key in &mut plan.order_by {
            self.pre_evaluate_uncorrelated_subqueries_in_expr(
                &mut sort_key.expression,
                outer_contexts,
                cte_store,
            )?;
        }
        Ok(())
    }

    fn pre_evaluate_uncorrelated_subqueries_in_expr(
        &mut self,
        expr: &mut crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<()> {
        match expr {
            crate::query::expr::Expr::Subquery(sub) => {
                if !sub.is_correlated && sub.materialized.is_none() {
                    let mat = self.evaluate_subquery_materialized_internal(
                        sub,
                        outer_contexts,
                        cte_store,
                    )?;
                    sub.materialized = Some(mat);
                }
            }
            crate::query::expr::Expr::Binary { left, right, .. } => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr(left, outer_contexts, cte_store)?;
                self.pre_evaluate_uncorrelated_subqueries_in_expr(
                    right,
                    outer_contexts,
                    cte_store,
                )?;
            }
            crate::query::expr::Expr::Unary { expr, .. }
            | crate::query::expr::Expr::IsNull(expr)
            | crate::query::expr::Expr::IsNotNull(expr) => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr(expr, outer_contexts, cte_store)?;
            }
            crate::query::expr::Expr::InList { expr, list, .. } => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr(expr, outer_contexts, cte_store)?;
                for item in list {
                    self.pre_evaluate_uncorrelated_subqueries_in_expr(
                        item,
                        outer_contexts,
                        cte_store,
                    )?;
                }
            }
            crate::query::expr::Expr::Literal(_)
            | crate::query::expr::Expr::Column(_)
            | crate::query::expr::Expr::OuterColumn { .. } => {}
        }
        Ok(())
    }

    /// Evaluates a subquery into a materialized in-memory representation.
    pub fn evaluate_subquery_materialized(
        &mut self,
        subquery: &crate::query::expr::SubqueryExpr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<crate::query::expr::MaterializedSubqueryResult> {
        let mut cte_store = std::collections::HashMap::new();
        self.evaluate_subquery_materialized_internal(subquery, outer_contexts, &mut cte_store)
    }

    fn evaluate_subquery_materialized_internal(
        &mut self,
        subquery: &crate::query::expr::SubqueryExpr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<crate::query::expr::MaterializedSubqueryResult> {
        match &subquery.kind {
            crate::query::expr::SubqueryKind::Scalar { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                if res.rows.is_empty() {
                    Ok(crate::query::expr::MaterializedSubqueryResult::Scalar(
                        Value::Null,
                    ))
                } else if res.rows.len() == 1 {
                    Ok(crate::query::expr::MaterializedSubqueryResult::Scalar(
                        res.rows[0].values[0].clone(),
                    ))
                } else {
                    Err(Error::ScalarSubqueryMultipleRows)
                }
            }
            crate::query::expr::SubqueryKind::In { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                let in_vals: Vec<Value> =
                    res.rows.into_iter().map(|r| r.values[0].clone()).collect();
                Ok(crate::query::expr::MaterializedSubqueryResult::In(in_vals))
            }
            crate::query::expr::SubqueryKind::Exists { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                let exists = !res.rows.is_empty();
                Ok(crate::query::expr::MaterializedSubqueryResult::Exists(
                    exists,
                ))
            }
        }
    }

    /// Evaluates an expression against a row in the current table and any enclosing outer scopes.
    pub fn evaluate_expr_with_outer(
        &mut self,
        expr: &crate::query::expr::Expr,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<Value> {
        let mut cte_store = std::collections::HashMap::new();
        self.evaluate_expr_with_outer_internal(expr, row, schema, outer_contexts, &mut cte_store)
    }

    fn evaluate_expr_with_outer_internal(
        &mut self,
        expr: &crate::query::expr::Expr,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Value> {
        match expr {
            crate::query::expr::Expr::Subquery(sub) => {
                if let Some(mat) = &sub.materialized {
                    match mat {
                        crate::query::expr::MaterializedSubqueryResult::Scalar(val) => {
                            Ok(val.clone())
                        }
                        crate::query::expr::MaterializedSubqueryResult::In(in_vals) => {
                            if let crate::query::expr::SubqueryKind::In { expr, negated } =
                                &sub.kind
                            {
                                let left = self.evaluate_expr_with_outer_internal(
                                    expr,
                                    row,
                                    schema,
                                    outer_contexts,
                                    cte_store,
                                )?;
                                Ok(crate::query::expr::eval_in_3vl(&left, in_vals, *negated))
                            } else {
                                unreachable!()
                            }
                        }
                        crate::query::expr::MaterializedSubqueryResult::Exists(b) => {
                            if let crate::query::expr::SubqueryKind::Exists { negated } = &sub.kind
                            {
                                let res = if *negated { !*b } else { *b };
                                Ok(Value::Boolean(res))
                            } else {
                                unreachable!()
                            }
                        }
                    }
                } else {
                    let mut nested = outer_contexts.to_vec();
                    nested.push((row, schema));
                    match &sub.kind {
                        crate::query::expr::SubqueryKind::Scalar { .. } => {
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            if res.rows.is_empty() {
                                Ok(Value::Null)
                            } else if res.rows.len() == 1 {
                                Ok(res.rows[0].values[0].clone())
                            } else {
                                Err(Error::ScalarSubqueryMultipleRows)
                            }
                        }
                        crate::query::expr::SubqueryKind::In { expr, negated } => {
                            let left = self.evaluate_expr_with_outer_internal(
                                expr,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            let in_vals: Vec<Value> =
                                res.rows.into_iter().map(|r| r.values[0].clone()).collect();
                            Ok(crate::query::expr::eval_in_3vl(&left, &in_vals, *negated))
                        }
                        crate::query::expr::SubqueryKind::Exists { negated } => {
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            let has_rows = !res.rows.is_empty();
                            let b = if *negated { !has_rows } else { has_rows };
                            Ok(Value::Boolean(b))
                        }
                    }
                }
            }
            crate::query::expr::Expr::InList {
                expr,
                list,
                negated,
            } => {
                let target = self.evaluate_expr_with_outer_internal(
                    expr,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                let mut vals = Vec::with_capacity(list.len());
                for item in list {
                    vals.push(self.evaluate_expr_with_outer_internal(
                        item,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?);
                }
                Ok(crate::query::expr::eval_in_3vl(&target, &vals, *negated))
            }
            crate::query::expr::Expr::Binary { left, op, right } => match op {
                crate::query::expr::BinaryOp::And => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    match l {
                        Value::Boolean(false) => Ok(Value::Boolean(false)),
                        Value::Boolean(true) => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
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
                crate::query::expr::BinaryOp::Or => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    match l {
                        Value::Boolean(true) => Ok(Value::Boolean(true)),
                        Value::Boolean(false) => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
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
                _ => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    let r = self.evaluate_expr_with_outer_internal(
                        right,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    let tmp_schema = Schema::new_projected(Vec::new());
                    let tmp_row = Row::new(Vec::new());
                    let bin_expr = crate::query::expr::Expr::Binary {
                        left: Box::new(crate::query::expr::Expr::Literal(l)),
                        op: *op,
                        right: Box::new(crate::query::expr::Expr::Literal(r)),
                    };
                    bin_expr.evaluate(&tmp_row, &tmp_schema)
                }
            },
            crate::query::expr::Expr::Unary { op, expr } => {
                let val = self.evaluate_expr_with_outer_internal(
                    expr,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                let tmp_schema = Schema::new_projected(Vec::new());
                let tmp_row = Row::new(Vec::new());
                let un_expr = crate::query::expr::Expr::Unary {
                    op: *op,
                    expr: Box::new(crate::query::expr::Expr::Literal(val)),
                };
                un_expr.evaluate(&tmp_row, &tmp_schema)
            }
            crate::query::expr::Expr::IsNull(inner) => {
                let val = self.evaluate_expr_with_outer_internal(
                    inner,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                Ok(Value::Boolean(val.is_null()))
            }
            crate::query::expr::Expr::IsNotNull(inner) => {
                let val = self.evaluate_expr_with_outer_internal(
                    inner,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                Ok(Value::Boolean(!val.is_null()))
            }
            crate::query::expr::Expr::Literal(_)
            | crate::query::expr::Expr::Column(_)
            | crate::query::expr::Expr::OuterColumn { .. } => {
                expr.evaluate_with_outer(row, schema, outer_contexts)
            }
        }
    }

    /// Filters rows evaluating predicate against active row and outer contexts.
    pub fn filter_rows_with_outer(
        &mut self,
        rows: Vec<Row>,
        schema: &Schema,
        predicate: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<Vec<Row>> {
        let mut cte_store = std::collections::HashMap::new();
        self.filter_rows_with_outer_internal(
            rows,
            schema,
            predicate,
            outer_contexts,
            &mut cte_store,
        )
    }

    fn filter_rows_with_outer_internal(
        &mut self,
        rows: Vec<Row>,
        schema: &Schema,
        predicate: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Vec<Row>> {
        let pred_type = predicate.infer_type(schema)?;
        if pred_type != DataType::Boolean && pred_type != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: pred_type.to_string(),
            });
        }

        let mut matched = Vec::new();
        for row in rows {
            let val = self.evaluate_expr_with_outer_internal(
                predicate,
                &row,
                schema,
                outer_contexts,
                cte_store,
            )?;
            if val == Value::Boolean(true) {
                matched.push(row);
            }
        }
        Ok(matched)
    }

    /// Performs nested-loop join evaluating join condition against combined row and outer contexts.
    #[allow(clippy::too_many_arguments)]
    pub fn join_rows_with_outer(
        &mut self,
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: crate::sql::ast::JoinType,
        on: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<(Schema, Vec<Row>)> {
        let mut cte_store = std::collections::HashMap::new();
        self.join_rows_with_outer_internal(
            left_rows,
            left_schema,
            right_rows,
            right_schema,
            join_type,
            on,
            outer_contexts,
            &mut cte_store,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn join_rows_with_outer_internal(
        &mut self,
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: crate::sql::ast::JoinType,
        on: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(Schema, Vec<Row>)> {
        let mut combined_columns = left_schema.columns.clone();
        for col in &right_schema.columns {
            let mut right_col = col.clone();
            if join_type == crate::sql::ast::JoinType::Left {
                right_col.nullable = true;
            }
            combined_columns.push(right_col);
        }
        let combined_schema = Schema::new_projected(combined_columns);

        let right_null_values = vec![Value::Null; right_schema.columns.len()];
        let mut result_rows = Vec::new();

        for l_row in left_rows {
            let mut matched = false;
            for r_row in right_rows {
                let mut combined_vals = Vec::with_capacity(l_row.values.len() + r_row.values.len());
                combined_vals.extend_from_slice(&l_row.values);
                combined_vals.extend_from_slice(&r_row.values);
                let candidate_row = Row::new(combined_vals);

                let on_val = self.evaluate_expr_with_outer_internal(
                    on,
                    &candidate_row,
                    &combined_schema,
                    outer_contexts,
                    cte_store,
                )?;
                if on_val == Value::Boolean(true) {
                    matched = true;
                    result_rows.push(candidate_row);
                }
            }
            if !matched && join_type == crate::sql::ast::JoinType::Left {
                let mut combined_vals =
                    Vec::with_capacity(l_row.values.len() + right_null_values.len());
                combined_vals.extend_from_slice(&l_row.values);
                combined_vals.extend_from_slice(&right_null_values);
                result_rows.push(Row::new(combined_vals));
            }
        }
        Ok((combined_schema, result_rows))
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
