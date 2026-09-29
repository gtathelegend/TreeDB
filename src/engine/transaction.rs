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
    if let Some(with_clause) = &stmt.with {
        for cte in &with_clause.ctes {
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, target_table)
                }
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, target_table)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, target_table)
                        || ast_references_table(recursive, target_table)
                }
            };
            if refs {
                return true;
            }
        }
    }
    if stmt.from.name() == target_table || stmt.from.alias() == Some(target_table) {
        return true;
    }
    for join in &stmt.joins {
        if join.table.name() == target_table || join.table.alias() == Some(target_table) {
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

fn ast_set_query_references_table(query: &crate::sql::SetQuery, target_table: &str) -> bool {
    if let Some(with_clause) = &query.with {
        for cte in &with_clause.ctes {
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, target_table)
                }
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, target_table)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, target_table)
                        || ast_references_table(recursive, target_table)
                }
            };
            if refs {
                return true;
            }
        }
    }
    if ast_set_query_body_references_table(&query.body, target_table) {
        return true;
    }
    for item in &query.order_by {
        if ast_expr_references_table(&item.expr, target_table) {
            return true;
        }
    }
    false
}

fn ast_set_query_body_references_table(
    body: &crate::sql::SetQueryBody,
    target_table: &str,
) -> bool {
    match body {
        crate::sql::SetQueryBody::Select(s) => ast_references_table(s, target_table),
        crate::sql::SetQueryBody::Parenthesized(q) => {
            ast_set_query_references_table(q, target_table)
        }
        crate::sql::SetQueryBody::SetOperation { left, right, .. } => {
            ast_set_query_body_references_table(left, target_table)
                || ast_set_query_body_references_table(right, target_table)
        }
    }
}

fn ast_expr_references_table(expr: &crate::sql::SqlExpr, target_table: &str) -> bool {
    match expr {
        crate::sql::SqlExpr::QualifiedIdentifier { qualifier, .. } => qualifier == target_table,
        crate::sql::SqlExpr::Identifier(_)
        | crate::sql::SqlExpr::Literal(_)
        | crate::sql::SqlExpr::Parameter(_)
        | crate::sql::SqlExpr::CountAll => false,
        crate::sql::SqlExpr::Subquery(sub) => ast_references_table(sub, target_table),
        crate::sql::SqlExpr::SetSubquery(sub) => ast_set_query_references_table(sub, target_table),
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
        crate::sql::SqlExpr::InSetSubquery { expr, subquery, .. } => {
            ast_expr_references_table(expr, target_table)
                || ast_set_query_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::Exists { subquery, .. } => {
            ast_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::SetExists { subquery, .. } => {
            ast_set_query_references_table(subquery, target_table)
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
    if let Some(set_op) = &plan.set_op {
        return plan_references_table(&set_op.left, target_table)
            || plan_references_table(&set_op.right, target_table);
    }
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
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, &other.name)
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

    fn get_index(&self, index_name: &str) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        let mut bp = self.bp.borrow_mut();
        crate::engine::catalog::Catalog::get_index(&mut bp, index_name)
    }
}

/// Execution result returned by top-level SQL statements (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`).
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
    /// Result of an SQL `CREATE INDEX` statement.
    CreatedIndex {
        /// Name of the created index.
        index: String,
    },
    /// Result of an SQL `DROP INDEX` statement.
    DroppedIndex {
        /// Name of the dropped index.
        index: String,
    },
    /// Result of an SQL `ANALYZE` statement.
    AnalyzedTable {
        /// Name of the analyzed table.
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
            StatementResult::CreatedTable { .. }
            | StatementResult::DroppedTable { .. }
            | StatementResult::CreatedIndex { .. }
            | StatementResult::DroppedIndex { .. }
            | StatementResult::AnalyzedTable { .. } => 0,
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
    pub(crate) has_catalog_mutations: bool,
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
            has_catalog_mutations: false,
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
        let res =
            crate::engine::catalog::Catalog::create_table(&mut self.db.buffer_pool, name, schema);
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
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
        self.create_composite_index(table_name, index_name, &[column_name], unique)
    }

    /// Creates a new persistent composite secondary index within this transaction.
    pub fn create_composite_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_names: &[&str],
        unique: bool,
    ) -> Result<crate::engine::catalog::IndexId> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::create_composite_index(
            &mut self.db.buffer_pool,
            table_name,
            index_name,
            column_names,
            unique,
        );
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.index_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
    }

    /// Drops a persistent secondary index and reclaims its pages within this transaction.
    pub fn drop_index(&mut self, index_name: &str) -> Result<()> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::drop_index(&mut self.db.buffer_pool, index_name);
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.index_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
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
        self.plan_select_with_outer_ctes_and_params(ast, &[], None)
    }

    /// Plans a parsed `SelectStatement` AST with supplied parameter values.
    pub fn plan_select_with_params(
        &mut self,
        ast: &crate::sql::SelectStatement,
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_select_with_outer_ctes_and_params(ast, &[], params)
    }

    fn bind_and_validate_ctes_with_params(
        &mut self,
        ctes: &[crate::sql::Cte],
        visible_ctes: &mut Vec<crate::sql::BoundCte>,
        params: Option<&[Value]>,
    ) -> Result<()> {
        validate_ast_cte_acyclicity(ctes)?;
        let mut declared_names = std::collections::HashSet::new();
        for cte in ctes {
            if !declared_names.insert(&cte.name) {
                return Err(Error::DuplicateCteName(cte.name.clone()));
            }

            match &cte.query {
                crate::sql::CteQuery::NonRecursive(select_stmt) => {
                    // Bind and plan the CTE query using currently visible CTEs
                    let cte_plan = self.plan_select_with_outer_ctes_and_params(
                        select_stmt,
                        visible_ctes,
                        params,
                    )?;

                    // Derive the CTE output schema from cte_plan projections
                    let projected_schema =
                        self.derive_cte_output_schema(&cte_plan, visible_ctes)?;

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
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
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
                    collect_cte_dependencies(&cte_plan, visible_ctes, &mut dependencies);

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
                crate::sql::CteQuery::SetOperation(set_query) => {
                    let cte_plan = self.plan_set_query_with_outer_ctes_and_params(
                        set_query,
                        visible_ctes,
                        params,
                    )?;
                    let projected_schema =
                        self.derive_cte_output_schema(&cte_plan, visible_ctes)?;

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
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
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

                    let mut dependencies = Vec::new();
                    collect_cte_dependencies(&cte_plan, visible_ctes, &mut dependencies);

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
                        self.plan_select_with_outer_ctes_and_params(anchor, visible_ctes, params)?;
                    let projected_schema =
                        self.derive_cte_output_schema(&anchor_plan, visible_ctes)?;

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
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
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
                    let rec_plan = self.plan_select_with_outer_ctes_and_params(
                        recursive,
                        &rec_visible_ctes,
                        params,
                    )?;

                    // 6. Check unsupported recursive aggregation or window functions
                    let self_ref_in_rec = plan_references_table(&rec_plan, &cte.name);
                    if self_ref_in_rec {
                        if !rec_plan.group_by.is_empty() || !rec_plan.aggregates.is_empty() {
                            return Err(Error::UnsupportedRecursiveAggregation(cte.name.clone()));
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
                    collect_cte_dependencies(&anchor_plan, visible_ctes, &mut dependencies);
                    collect_cte_dependencies(&rec_plan, visible_ctes, &mut dependencies);

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
        Ok(())
    }

    /// Plans a parsed `SelectStatement` AST with in-scope outer CTE definitions.
    pub fn plan_select_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.plan_select_with_outer_ctes_and_params(ast, outer_ctes, None)
    }

    /// Plans a parsed `SelectStatement` AST with in-scope outer CTE definitions and positional parameters.
    pub fn plan_select_with_outer_ctes_and_params(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let mut visible_ctes: Vec<crate::sql::BoundCte> = outer_ctes.to_vec();

        if let Some(with_clause) = &ast.with {
            self.bind_and_validate_ctes_with_params(&with_clause.ctes, &mut visible_ctes, params)?;
        }

        let rewrite_res = {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: Some(&visible_ctes),
            };
            crate::query::QueryRewriter::rewrite_select(ast, Some(&catalog))?
        };
        let rewritten_ast = rewrite_res.statement;
        let rewrite_logs = rewrite_res.rewrites;

        // If the FROM or JOIN relations contain non-flattened derived tables, plan them as statement-local CTEs
        if let crate::sql::ast::TableRef::Derived { query, alias } = &rewritten_ast.from {
            let inner_plan = match query {
                crate::sql::ast::DerivedQuery::Select(select_stmt) => {
                    self.plan_select_with_outer_ctes_and_params(select_stmt, &visible_ctes, params)?
                }
                crate::sql::ast::DerivedQuery::SetOperation(set_stmt) => {
                    self.plan_set_query_with_outer_ctes_and_params(set_stmt, &visible_ctes, params)?
                }
            };
            let output_schema = self.derive_cte_output_schema(&inner_plan, &visible_ctes)?;
            visible_ctes.push(crate::sql::binder::BoundCte {
                name: alias.clone(),
                output_schema,
                kind: crate::sql::binder::BoundCteKind::NonRecursive { plan: inner_plan },
                dependencies: Vec::new(),
            });
        }

        for j in &rewritten_ast.joins {
            if let crate::sql::ast::TableRef::Derived { query, alias } = &j.table {
                let inner_plan = match query {
                    crate::sql::ast::DerivedQuery::Select(select_stmt) => self
                        .plan_select_with_outer_ctes_and_params(
                            select_stmt,
                            &visible_ctes,
                            params,
                        )?,
                    crate::sql::ast::DerivedQuery::SetOperation(set_stmt) => self
                        .plan_set_query_with_outer_ctes_and_params(
                            set_stmt,
                            &visible_ctes,
                            params,
                        )?,
                };
                let output_schema = self.derive_cte_output_schema(&inner_plan, &visible_ctes)?;
                visible_ctes.push(crate::sql::binder::BoundCte {
                    name: alias.clone(),
                    output_schema,
                    kind: crate::sql::binder::BoundCteKind::NonRecursive { plan: inner_plan },
                    dependencies: Vec::new(),
                });
            }
        }

        // Now plan the main query body with visible_ctes
        let mut table_infos = Vec::new();
        let mut bindings = Vec::new();

        // 1. Resolve FROM table
        {
            let from_name = rewritten_ast.from.name();
            let from_alias = rewritten_ast.from.alias();
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(from_name, &visible_ctes)?;
            let stats = if is_cte {
                None
            } else {
                self.db.get_statistics(from_name).cloned()
            };
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(from_name, from_alias, schema, indexes)
                    .with_cte(is_cte)
                    .with_stats(stats),
            );
        }

        // 2. Resolve JOIN tables
        for j in &rewritten_ast.joins {
            let join_name = j.table.name();
            let join_alias = j.table.alias();
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(join_name, &visible_ctes)?;
            let stats = if is_cte {
                None
            } else {
                self.db.get_statistics(join_name).cloned()
            };
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(join_name, join_alias, schema, indexes)
                    .with_cte(is_cte)
                    .with_stats(stats),
            );
        }

        for (idx, t) in table_infos.iter().enumerate() {
            let is_semi_or_anti = if idx == 0 {
                false
            } else {
                let j = &rewritten_ast.joins[idx - 1];
                j.join_type == crate::sql::ast::JoinType::Semi
                    || j.join_type == crate::sql::ast::JoinType::Anti
            };
            bindings.push(
                crate::sql::TableBinding::new(&t.name, t.alias.as_deref(), &t.schema)
                    .with_semi_or_anti(is_semi_or_anti),
            );
        }

        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
        let catalog = TxnBufferPoolCatalog {
            bp: &bp_cell,
            ctes: Some(&visible_ctes),
        };
        let mut bound = crate::sql::binder::bind_select_multi_with_catalog_and_params(
            &rewritten_ast,
            &bindings,
            Some(&catalog),
            params,
        )?;
        bound.ctes = visible_ctes;
        let mut plan = crate::query::optimizer::Optimizer::optimize(&bound, &table_infos)?;
        plan.logical_rewrites = rewrite_logs;
        Ok(plan)
    }

    /// Plans a parsed `SetQuery` AST into an optimized physical execution plan (`PhysicalPlan`).
    pub fn plan_set_query(
        &mut self,
        ast: &crate::sql::SetQuery,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_set_query_with_outer_ctes_and_params(ast, &[], None)
    }

    /// Plans a parsed `SetQuery` AST with supplied parameter values.
    pub fn plan_set_query_with_params(
        &mut self,
        ast: &crate::sql::SetQuery,
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_set_query_with_outer_ctes_and_params(ast, &[], params)
    }

    /// Plans a parsed `SetQuery` AST with in-scope outer CTE definitions.
    pub fn plan_set_query_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SetQuery,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.plan_set_query_with_outer_ctes_and_params(ast, outer_ctes, None)
    }

    /// Plans a parsed `SetQuery` AST with in-scope outer CTE definitions and positional parameters.
    pub fn plan_set_query_with_outer_ctes_and_params(
        &mut self,
        ast: &crate::sql::SetQuery,
        outer_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let mut visible_ctes: Vec<crate::sql::BoundCte> = outer_ctes.to_vec();

        if let Some(with_clause) = &ast.with {
            self.bind_and_validate_ctes_with_params(&with_clause.ctes, &mut visible_ctes, params)?;
        }

        let mut plan = self.plan_set_query_body_with_params(&ast.body, &visible_ctes, params)?;

        if !ast.order_by.is_empty() {
            let schema = self.derive_cte_output_schema(&plan, &visible_ctes)?;
            let mut bound_order_by = Vec::with_capacity(ast.order_by.len());
            for order_item in &ast.order_by {
                let bound_expr = match &order_item.expr {
                    crate::sql::SqlExpr::Identifier(ident) => {
                        if schema.columns.iter().any(|c| &c.name == ident) {
                            crate::query::expr::Expr::Column(ident.clone())
                        } else {
                            return Err(Error::ColumnNotFound(ident.clone()));
                        }
                    }
                    _ => {
                        let binding = crate::sql::TableBinding::new("", None, &schema);
                        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
                        let catalog = TxnBufferPoolCatalog {
                            bp: &bp_cell,
                            ctes: Some(&visible_ctes),
                        };
                        let mut ctx = crate::sql::binder::BindingContext::with_parent_and_catalog(
                            vec![binding],
                            None,
                            Some(&catalog),
                        );
                        ctx.params = params;
                        crate::sql::binder::bind_expr(&order_item.expr, &ctx)?
                    }
                };
                let direction = match order_item.direction {
                    crate::sql::OrderDirection::Asc => crate::query::SortDirection::Ascending,
                    crate::sql::OrderDirection::Desc => crate::query::SortDirection::Descending,
                };
                bound_order_by.push(crate::query::SortKey::new(bound_expr, direction));
            }
            plan.order_by = bound_order_by;
        }

        if let Some(limit) = ast.limit {
            plan.limit = Some(limit as usize);
        }
        if let Some(offset) = ast.offset {
            plan.offset = Some(offset as usize);
        }
        plan.ctes = visible_ctes;

        Ok(plan)
    }

    fn plan_set_query_body_with_params(
        &mut self,
        body: &crate::sql::SetQueryBody,
        visible_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        match body {
            crate::sql::SetQueryBody::Select(statement) => {
                self.plan_select_with_outer_ctes_and_params(statement, visible_ctes, params)
            }
            crate::sql::SetQueryBody::Parenthesized(inner) => {
                self.plan_set_query_with_outer_ctes_and_params(inner, visible_ctes, params)
            }
            crate::sql::SetQueryBody::SetOperation { op, left, right } => {
                let left_plan = self.plan_set_query_body_with_params(left, visible_ctes, params)?;
                let right_plan =
                    self.plan_set_query_body_with_params(right, visible_ctes, params)?;
                let left_schema = self.derive_cte_output_schema(&left_plan, visible_ctes)?;
                let right_schema = self.derive_cte_output_schema(&right_plan, visible_ctes)?;

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
                        crate::sql::binder::unify_set_types(l_col.data_type, r_col.data_type)
                            .ok_or_else(|| Error::SetOperationTypeMismatch {
                                col_idx: idx + 1,
                                left_type: format!("{:?}", l_col.data_type),
                                right_type: format!("{:?}", r_col.data_type),
                            })?;
                    unified_cols.push(crate::engine::types::Column::new(
                        l_col.name.clone(),
                        unified_type,
                        l_col.nullable || r_col.nullable,
                    ));
                }
                let unified_schema = crate::engine::types::Schema::new_projected(unified_cols);

                Ok(crate::query::optimizer::PhysicalPlan::new_set_op(
                    crate::query::optimizer::PhysicalSetOp {
                        op: *op,
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        schema: unified_schema,
                    },
                ))
            }
        }
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
        if let Some(set_op) = &plan.set_op {
            return Ok(set_op.schema.clone());
        }
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();

        let (base_schema, _, _) =
            self.resolve_table_for_planning(&plan.base_table.table_name, visible_ctes)?;

        let current_schema = if plan.joins.is_empty() {
            base_schema
        } else {
            let mut cols = Vec::new();
            let q = plan.base_table.qualifier();
            let base_is_nullable = plan.joins.iter().any(|j| {
                j.join_type == crate::sql::ast::JoinType::Right
                    || j.join_type == crate::sql::ast::JoinType::Full
            });
            for col in &base_schema.columns {
                cols.push(crate::engine::types::Column::new(
                    format!("{q}.{}", col.name),
                    col.data_type,
                    col.nullable || base_is_nullable,
                ));
            }
            for join in &plan.joins {
                if join.join_type == crate::sql::ast::JoinType::Semi
                    || join.join_type == crate::sql::ast::JoinType::Anti
                {
                    continue;
                }
                let (right_schema, _, _) =
                    self.resolve_table_for_planning(&join.right.table_name, visible_ctes)?;
                let rq = join.right.qualifier();
                for col in &right_schema.columns {
                    let mut right_col = col.clone();
                    if join.join_type == crate::sql::ast::JoinType::Left
                        || join.join_type == crate::sql::ast::JoinType::Full
                    {
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
    if let Some(set_op) = &plan.set_op {
        collect_cte_dependencies(&set_op.left, visible_ctes, deps);
        collect_cte_dependencies(&set_op.right, visible_ctes, deps);
        return;
    }
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
        | crate::query::expr::Expr::OuterColumn { .. }
        | crate::query::expr::Expr::Parameter { .. } => {}
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
            crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(&set_stmt),
            crate::sql::Statement::Explain(explain_stmt) => match explain_stmt.query.as_ref() {
                crate::sql::Statement::Select(select_stmt) => self.plan_select(select_stmt),
                crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(set_stmt),
                _ => Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    explain_stmt.query
                ))),
            },
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

    /// Executes an arbitrary top-level SQL statement (`SELECT`, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`, `EXPLAIN`)
    /// within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_statement(&mut self, sql: &str) -> Result<StatementResult> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Explain(explain_stmt) => {
                let plan = match explain_stmt.query.as_ref() {
                    crate::sql::Statement::Select(select_stmt) => self.plan_select(select_stmt)?,
                    crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(set_stmt)?,
                    _ => {
                        return Err(Error::UnsupportedExplainTarget(format!(
                            "{}",
                            explain_stmt.query
                        )))
                    }
                };
                let plan_str = if explain_stmt.analyze {
                    let start = std::time::Instant::now();
                    let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                    let metrics = crate::query::QueryExecutionMetrics::new(
                        start.elapsed(),
                        res.rows.len(),
                        Some(root_op),
                    );
                    crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                } else {
                    plan.to_string()
                };
                let schema = Schema::new_projected(vec![crate::engine::types::Column::new(
                    "plan",
                    DataType::String,
                    false,
                )]);
                let rows = vec![Row::new(vec![Value::String(plan_str)])];
                Ok(StatementResult::Query(crate::query::QueryResult::new(
                    schema, rows,
                )))
            }
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                let query_result = self.execute_physical_plan(&plan)?;
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = self.plan_set_query(&set_stmt)?;
                let query_result = self.execute_physical_plan(&plan)?;
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::CreateTable(create_stmt) => {
                self.execute_create_table(&create_stmt)
            }
            crate::sql::Statement::DropTable(drop_stmt) => self.execute_drop_table(&drop_stmt),
            crate::sql::Statement::CreateIndex(create_stmt) => {
                self.execute_create_index(&create_stmt)
            }
            crate::sql::Statement::DropIndex(drop_stmt) => self.execute_drop_index(&drop_stmt),
            crate::sql::Statement::Insert(insert_stmt) => self.execute_insert(&insert_stmt),
            crate::sql::Statement::Update(update_stmt) => self.execute_update(&update_stmt),
            crate::sql::Statement::Delete(delete_stmt) => self.execute_delete(&delete_stmt),
            crate::sql::Statement::Analyze(analyze_stmt) => self.execute_analyze(&analyze_stmt),
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

    /// Parses, binds, optimizes, and executes an SQL statement, returning both the query result
    /// and fine-grained runtime execution metrics.
    pub fn execute_sql_with_metrics(
        &mut self,
        sql: &str,
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        self.ensure_active()?;
        let start = std::time::Instant::now();
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                );
                Ok((qr, metrics))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = self.plan_set_query(&set_stmt)?;
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                );
                Ok((qr, metrics))
            }
            other => {
                let res = match other {
                    crate::sql::Statement::Explain(explain_stmt) => {
                        let plan = match explain_stmt.query.as_ref() {
                            crate::sql::Statement::Select(select_stmt) => {
                                self.plan_select(select_stmt)?
                            }
                            crate::sql::Statement::SetQuery(set_stmt) => {
                                self.plan_set_query(set_stmt)?
                            }
                            _ => {
                                return Err(Error::UnsupportedExplainTarget(format!(
                                    "{}",
                                    explain_stmt.query
                                )))
                            }
                        };
                        let plan_str = if explain_stmt.analyze {
                            let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                            let metrics = crate::query::QueryExecutionMetrics::new(
                                start.elapsed(),
                                res.rows.len(),
                                Some(root_op),
                            );
                            crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                        } else {
                            plan.to_string()
                        };
                        let schema =
                            Schema::new_projected(vec![crate::engine::types::Column::new(
                                "plan",
                                DataType::String,
                                false,
                            )]);
                        let rows = vec![Row::new(vec![Value::String(plan_str)])];
                        crate::query::QueryResult::new(schema, rows)
                    }
                    crate::sql::Statement::CreateTable(create_stmt) => {
                        self.execute_create_table(&create_stmt)?.into_query()?
                    }
                    crate::sql::Statement::DropTable(drop_stmt) => {
                        self.execute_drop_table(&drop_stmt)?.into_query()?
                    }
                    crate::sql::Statement::CreateIndex(create_stmt) => {
                        self.execute_create_index(&create_stmt)?.into_query()?
                    }
                    crate::sql::Statement::DropIndex(drop_stmt) => {
                        self.execute_drop_index(&drop_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Insert(insert_stmt) => {
                        self.execute_insert(&insert_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Update(update_stmt) => {
                        self.execute_update(&update_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Delete(delete_stmt) => {
                        self.execute_delete(&delete_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Analyze(analyze_stmt) => {
                        self.execute_analyze(&analyze_stmt)?.into_query()?
                    }
                    _ => unreachable!(),
                };
                let metrics =
                    crate::query::QueryExecutionMetrics::new(start.elapsed(), res.rows.len(), None);
                Ok((res, metrics))
            }
        }
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

    fn execute_create_index(
        &mut self,
        stmt: &crate::sql::CreateIndexStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let bound = {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: None,
            };
            crate::sql::binder::bind_create_index(stmt, &catalog)?
        };
        let cols_refs: Vec<&str> = bound.column_names.iter().map(|s| s.as_str()).collect();
        self.create_composite_index(&bound.table_name, &bound.name, &cols_refs, bound.unique)?;
        Ok(StatementResult::CreatedIndex { index: bound.name })
    }

    fn execute_drop_index(
        &mut self,
        stmt: &crate::sql::DropIndexStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: None,
            };
            crate::sql::binder::bind_drop_index(stmt, &catalog)?;
        }
        self.drop_index(&stmt.name)?;
        Ok(StatementResult::DroppedIndex {
            index: stmt.name.clone(),
        })
    }

    /// Prepares an SQL query string into a reusable `PreparedStatement` with inferred parameter metadata.
    pub fn prepare_sql(&mut self, sql: &str) -> Result<crate::sql::PreparedStatement> {
        self.ensure_active()?;
        let statement = crate::sql::parse_statement(sql)?;
        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
        let catalog = TxnBufferPoolCatalog {
            bp: &bp_cell,
            ctes: None,
        };
        let parameters = crate::sql::infer_statement_parameters(&statement, &catalog)?;
        Ok(crate::sql::PreparedStatement::new(
            sql, statement, parameters,
        ))
    }

    fn validate_prepared_params(
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<()> {
        if params.len() != stmt.parameter_count() {
            return Err(Error::ParameterCountMismatch {
                expected: stmt.parameter_count(),
                received: params.len(),
            });
        }
        for param_meta in stmt.parameters() {
            if let Some(expected_type) = param_meta.inferred_type {
                let val = &params[param_meta.index - 1];
                if !val.is_null() {
                    let val_type = val.data_type();
                    if val_type != expected_type {
                        let can_coerce = matches!(
                            (val, expected_type),
                            (Value::Int64(i), DataType::UInt64) if *i >= 0
                        ) || matches!(
                            (val, expected_type),
                            (Value::Int64(_), DataType::Float64)
                        ) || matches!(
                            (val, expected_type),
                            (Value::UInt64(u), DataType::Int64) if *u <= i64::MAX as u64
                        ) || matches!(
                            (val, expected_type),
                            (Value::UInt64(_), DataType::Float64)
                        );
                        if !can_coerce {
                            return Err(Error::ParameterTypeMismatch {
                                param_index: param_meta.index,
                                expected: expected_type.to_string(),
                                found: val_type.to_string(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Executes a reusable `PreparedStatement` with the provided parameter values within this transaction.
    pub fn execute_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;

        match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                let plan = if self.has_catalog_mutations {
                    self.plan_select_with_params(select_stmt, Some(params))?
                } else {
                    let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    ) {
                        cached_plan
                    } else {
                        let new_plan = self.plan_select_with_params(select_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        );
                        self.db.plan_cache.insert(key, new_plan.clone(), validity);
                        new_plan
                    }
                };
                let query_result = self.execute_physical_plan(&plan)?;
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = if self.has_catalog_mutations {
                    self.plan_set_query_with_params(set_stmt, Some(params))?
                } else {
                    let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    ) {
                        cached_plan
                    } else {
                        let new_plan = self.plan_set_query_with_params(set_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        );
                        self.db.plan_cache.insert(key, new_plan.clone(), validity);
                        new_plan
                    }
                };
                let query_result = self.execute_physical_plan(&plan)?;
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::Insert(insert_stmt) => {
                self.execute_insert_with_params(insert_stmt, Some(params))
            }
            crate::sql::Statement::Update(update_stmt) => {
                self.execute_update_with_params(update_stmt, Some(params))
            }
            crate::sql::Statement::Delete(delete_stmt) => {
                self.execute_delete_with_params(delete_stmt, Some(params))
            }
            crate::sql::Statement::Explain(explain_stmt) => {
                let plan = match explain_stmt.query.as_ref() {
                    crate::sql::Statement::Select(select_stmt) => {
                        self.plan_select_with_params(select_stmt, Some(params))?
                    }
                    crate::sql::Statement::SetQuery(set_stmt) => {
                        self.plan_set_query_with_params(set_stmt, Some(params))?
                    }
                    _ => {
                        return Err(Error::UnsupportedExplainTarget(format!(
                            "{}",
                            explain_stmt.query
                        )))
                    }
                };
                let plan_str = if explain_stmt.analyze {
                    let start = std::time::Instant::now();
                    let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                    let metrics = crate::query::QueryExecutionMetrics::new(
                        start.elapsed(),
                        res.rows.len(),
                        Some(root_op),
                    );
                    crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                } else {
                    plan.to_string()
                };
                let schema = Schema::new_projected(vec![crate::engine::types::Column::new(
                    "plan",
                    DataType::String,
                    false,
                )]);
                let rows = vec![Row::new(vec![Value::String(plan_str)])];
                Ok(StatementResult::Query(crate::query::QueryResult::new(
                    schema, rows,
                )))
            }
            crate::sql::Statement::CreateTable(create_stmt) => {
                self.execute_create_table(create_stmt)
            }
            crate::sql::Statement::DropTable(drop_stmt) => self.execute_drop_table(drop_stmt),
            crate::sql::Statement::CreateIndex(create_stmt) => {
                self.execute_create_index(create_stmt)
            }
            crate::sql::Statement::DropIndex(drop_stmt) => self.execute_drop_index(drop_stmt),
            crate::sql::Statement::Analyze(analyze_stmt) => self.execute_analyze(analyze_stmt),
        }
    }

    /// Executes a prepared query statement returning a `QueryResult`.
    pub fn execute_prepared_query(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<crate::query::QueryResult> {
        let res = self.execute_prepared(stmt, params)?;
        res.into_query()
    }

    /// Executes a prepared query statement returning both the query result and runtime metrics.
    pub fn execute_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let start = std::time::Instant::now();

        match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                let (plan, is_hit) = if self.has_catalog_mutations {
                    (
                        self.plan_select_with_params(select_stmt, Some(params))?,
                        false,
                    )
                } else {
                    let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    ) {
                        (cached_plan, true)
                    } else {
                        let new_plan = self.plan_select_with_params(select_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        );
                        self.db.plan_cache.insert(key, new_plan.clone(), validity);
                        (new_plan, false)
                    }
                };
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                )
                .with_plan_cache_hit(is_hit);
                Ok((qr, metrics))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let (plan, is_hit) = if self.has_catalog_mutations {
                    (
                        self.plan_set_query_with_params(set_stmt, Some(params))?,
                        false,
                    )
                } else {
                    let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    ) {
                        (cached_plan, true)
                    } else {
                        let new_plan = self.plan_set_query_with_params(set_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        );
                        self.db.plan_cache.insert(key, new_plan.clone(), validity);
                        (new_plan, false)
                    }
                };
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                )
                .with_plan_cache_hit(is_hit);
                Ok((qr, metrics))
            }
            _ => {
                let res = self.execute_prepared(stmt, params)?.into_query()?;
                let metrics =
                    crate::query::QueryExecutionMetrics::new(start.elapsed(), res.rows.len(), None);
                Ok((res, metrics))
            }
        }
    }

    /// Explains the physical execution plan for a prepared statement with concrete parameters.
    pub fn explain_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let plan = match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                self.plan_select_with_params(select_stmt, Some(params))?
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                self.plan_set_query_with_params(set_stmt, Some(params))?
            }
            _ => {
                return Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    stmt.statement
                )))
            }
        };
        Ok(plan.to_string())
    }

    /// Explains and analyzes the physical execution plan for a prepared statement with concrete parameters.
    pub fn explain_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let start = std::time::Instant::now();
        let plan = match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                self.plan_select_with_params(select_stmt, Some(params))?
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                self.plan_set_query_with_params(set_stmt, Some(params))?
            }
            _ => {
                return Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    stmt.statement
                )))
            }
        };
        let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
        let metrics = crate::query::QueryExecutionMetrics::new(
            start.elapsed(),
            res.rows.len(),
            Some(root_op),
        );
        Ok(crate::query::metrics::format_plan_with_metrics(
            &plan, &metrics,
        ))
    }

    fn execute_insert(&mut self, stmt: &crate::sql::InsertStatement) -> Result<StatementResult> {
        self.execute_insert_with_params(stmt, None)
    }

    fn execute_insert_with_params(
        &mut self,
        stmt: &crate::sql::InsertStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
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
                let mut ctx =
                    crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema);
                ctx.params = params;
                let bound_expr = crate::sql::bind_expr(expr, &ctx)?;
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
        self.execute_update_with_params(stmt, None)
    }

    fn execute_update_with_params(
        &mut self,
        stmt: &crate::sql::UpdateStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
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

            let mut ctx = crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema);
            ctx.params = params;
            let bound_expr = crate::sql::bind_expr(&assign.expr, &ctx)?;
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
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (crate::query::expr::Expr::Parameter { value: Value::Int64(i), .. }, DataType::UInt64) if *i >= 0
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (
                        crate::query::expr::Expr::Parameter {
                            value: Value::Int64(_),
                            ..
                        },
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
        let mut plan = self.plan_select_with_params(&select_ast, params)?;

        // 3. Fetch candidate rows
        let candidate_rows = if !plan.joins.is_empty() {
            let qr = self.execute_physical_plan(&plan)?;
            qr.rows
        } else {
            self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
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
                    crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
                    | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                        table.composite_index_lookup(index_name, vals)?
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
        self.execute_delete_with_params(stmt, None)
    }

    fn execute_delete_with_params(
        &mut self,
        stmt: &crate::sql::DeleteStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
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
        let mut plan = self.plan_select_with_params(&select_ast, params)?;

        // 2. Fetch candidate rows
        let candidate_rows = if !plan.joins.is_empty() {
            let qr = self.execute_physical_plan(&plan)?;
            qr.rows
        } else {
            self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
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
                    crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
                    | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                        table.composite_index_lookup(index_name, vals)?
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

    fn execute_analyze(&mut self, stmt: &crate::sql::AnalyzeStatement) -> Result<StatementResult> {
        self.ensure_active()?;
        self.collect_statistics(&stmt.table_name)?;
        Ok(StatementResult::AnalyzedTable {
            table: stmt.table_name.clone(),
        })
    }

    /// Derives and registers runtime statistics for the named table within this transaction.
    pub fn collect_statistics(
        &mut self,
        table_name: &str,
    ) -> Result<crate::query::stats::TableStatistics> {
        self.ensure_active()?;
        let mut table = self.open_table(table_name)?;
        let stats = table.collect_statistics()?;
        self.db.set_statistics(stats.clone());
        Ok(stats)
    }

    /// Retrieves registered statistics for a table if available.
    pub fn get_statistics(
        &self,
        table_name: &str,
    ) -> Option<&crate::query::stats::TableStatistics> {
        self.db.get_statistics(table_name)
    }

    /// Sets or overrides statistics for a table.
    pub fn set_statistics(&mut self, stats: crate::query::stats::TableStatistics) {
        self.db.set_statistics(stats);
    }

    /// Removes registered statistics for a table.
    pub fn clear_statistics(&mut self, table_name: &str) {
        self.db.clear_statistics(table_name);
    }

    /// Returns a reference to the active external sort configuration.
    pub fn sort_config(&self) -> &crate::query::external_sort::SortConfig {
        self.db.sort_config()
    }

    /// Returns a mutable reference to the active external sort configuration.
    pub fn sort_config_mut(&mut self) -> &mut crate::query::external_sort::SortConfig {
        self.db.sort_config_mut()
    }

    /// Sets the active external sort configuration.
    pub fn set_sort_config(&mut self, config: crate::query::external_sort::SortConfig) {
        self.db.set_sort_config(config);
    }

    /// Returns a reference to the active external aggregation configuration.
    pub fn aggregate_config(&self) -> &crate::query::external_aggregate::AggregateConfig {
        self.db.aggregate_config()
    }

    /// Returns a mutable reference to the active external aggregation configuration.
    pub fn aggregate_config_mut(
        &mut self,
    ) -> &mut crate::query::external_aggregate::AggregateConfig {
        self.db.aggregate_config_mut()
    }

    /// Sets the active external aggregation configuration.
    pub fn set_aggregate_config(
        &mut self,
        config: crate::query::external_aggregate::AggregateConfig,
    ) {
        self.db.set_aggregate_config(config);
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
        let (qr, _metrics) =
            self.execute_physical_plan_internal_with_metrics(plan, outer_contexts, &mut cte_store)?;
        Ok(qr)
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction, returning both the query result
    /// and the full operator execution metrics tree.
    pub fn execute_physical_plan_with_metrics(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let mut cte_store = std::collections::HashMap::new();
        self.execute_physical_plan_internal_with_metrics(plan, &[], &mut cte_store)
    }

    fn execute_physical_plan_internal(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<crate::query::QueryResult> {
        let (res, _metrics) =
            self.execute_physical_plan_internal_with_metrics(plan, outer_contexts, cte_store)?;
        Ok(res)
    }

    fn execute_physical_plan_internal_with_metrics(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        self.ensure_active()?;
        let op_start = std::time::Instant::now();

        // 0. Materialize any CTEs defined on this plan (in declaration order)
        let mut cte_metrics = Vec::new();
        for cte in &plan.ctes {
            if !cte_store.contains_key(&cte.name) {
                match &cte.kind {
                    crate::sql::BoundCteKind::NonRecursive { plan: cte_plan } => {
                        let (res, cte_op) = self.execute_physical_plan_internal_with_metrics(
                            cte_plan,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut cte_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Cte {
                                name: cte.name.clone(),
                            },
                            Some(cte_plan.estimated_rows),
                            Some(cte_plan.total_cost),
                        );
                        cte_metric.actual_rows = res.rows.len();
                        cte_metric.input_rows = res.rows.len();
                        cte_metric.add_child(cte_op);
                        cte_metrics.push(cte_metric);
                        cte_store.insert(cte.name.clone(), (cte.output_schema.clone(), res.rows));
                    }
                    crate::sql::BoundCteKind::Recursive {
                        anchor_plan,
                        recursive_plan,
                        max_iterations,
                        max_rows,
                    } => {
                        let (anchor_res, anchor_op) = self
                            .execute_physical_plan_internal_with_metrics(
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

                            let (next_res, _next_op) = self
                                .execute_physical_plan_internal_with_metrics(
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
                            (cte.output_schema.clone(), accumulated_rows.clone()),
                        );

                        let mut rec_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::RecursiveCte {
                                name: cte.name.clone(),
                                iterations,
                            },
                            Some(anchor_plan.estimated_rows),
                            Some(anchor_plan.total_cost),
                        );
                        rec_metric.actual_rows = accumulated_rows.len();
                        rec_metric.input_rows = accumulated_rows.len();
                        rec_metric.add_child(anchor_op);
                        cte_metrics.push(rec_metric);
                    }
                }
            }
        }

        let mut plan = plan.clone();
        self.pre_evaluate_uncorrelated_subqueries_internal(&mut plan, outer_contexts, cte_store)?;

        if let Some(set_op) = &plan.set_op {
            let (left_res, left_op) = self.execute_physical_plan_internal_with_metrics(
                &set_op.left,
                outer_contexts,
                cte_store,
            )?;
            let (right_res, right_op) = self.execute_physical_plan_internal_with_metrics(
                &set_op.right,
                outer_contexts,
                cte_store,
            )?;
            let left_count = left_res.rows.len();
            let right_count = right_res.rows.len();
            let set_rows = crate::query::SetOperationOperator::apply(
                left_res.rows,
                right_res.rows,
                &set_op.schema,
                &set_op.op,
            )?;
            let mut set_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::SetOperation {
                    op: set_op.op.to_string(),
                },
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            set_metric.input_rows = left_count + right_count;
            set_metric.actual_rows = set_rows.len();
            set_metric.add_child(left_op);
            set_metric.add_child(right_op);

            let mut current_op = set_metric;
            let mut current_rows = set_rows;

            if !plan.order_by.is_empty() {
                let (sorted_rows, sort_metrics) = crate::query::SortOperator::apply_with_config(
                    current_rows,
                    &set_op.schema,
                    &plan.order_by,
                    &self.db.sort_config,
                )?;
                let mut sort_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::Sort,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                sort_metric.input_rows = sort_metrics.rows_input;
                sort_metric.actual_rows = sorted_rows.len();
                sort_metric.counters.runs_created = sort_metrics.runs_created;
                sort_metric.counters.bytes_spilled = sort_metrics.bytes_spilled;
                sort_metric.counters.merge_passes = sort_metrics.merge_passes;
                sort_metric.counters.peak_memory_bytes = sort_metrics.peak_memory_bytes;
                sort_metric.add_child(current_op);
                current_op = sort_metric;
                current_rows = sorted_rows;
            }

            if plan.offset.is_some() || plan.limit.is_some() {
                let paginated_rows =
                    crate::query::OffsetLimitOperator::apply(current_rows, plan.offset, plan.limit);
                let mut limit_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::LimitOffset,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                limit_metric.input_rows = current_op.actual_rows;
                limit_metric.actual_rows = paginated_rows.len();
                limit_metric.add_child(current_op);
                current_op = limit_metric;
                current_rows = paginated_rows;
            }

            current_op.execution_time = op_start.elapsed();
            return Ok((
                crate::query::QueryResult::new(set_op.schema.clone(), current_rows),
                current_op,
            ));
        }

        // 1. Execute base table scan
        let (mut current_schema, mut current_rows, mut current_op) = {
            if let Some((cte_schema, cte_rows)) = cte_store.get(&plan.base_table.table_name) {
                let raw_schema = cte_schema.clone();
                let initial_rows = cte_rows.clone();
                let mut scan_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TableScan {
                        table_name: plan.base_table.table_name.clone(),
                        access_path: "CTE Scan".to_string(),
                    },
                    Some(plan.base_table.estimated_rows),
                    Some(plan.base_table.cost),
                );
                scan_metric.input_rows = initial_rows.len();
                scan_metric.actual_rows = initial_rows.len();
                scan_metric.counters.tuples_fetched = initial_rows.len();
                scan_metric.counters.rows_evaluated = initial_rows.len();

                let (filtered_rows, table_op) = match &plan.base_table.residual_filter {
                    Some(filter) => {
                        let filtered = self.filter_rows_with_outer_internal(
                            initial_rows.clone(),
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        filter_metric.input_rows = initial_rows.len();
                        filter_metric.actual_rows = filtered.len();
                        filter_metric.counters.rows_evaluated = initial_rows.len();
                        filter_metric.counters.rows_passed = filtered.len();
                        filter_metric.counters.rows_rejected =
                            initial_rows.len().saturating_sub(filtered.len());
                        filter_metric.add_child(scan_metric);
                        (filtered, filter_metric)
                    }
                    None => (initial_rows, scan_metric),
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

                (schema, filtered_rows, table_op)
            } else {
                let mut table = self.open_table(&plan.base_table.table_name)?;
                let raw_schema = table.schema().clone();
                let mut index_lookups = 0;
                let initial_rows = match &plan.base_table.access_path {
                    crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                        index_lookups = 1;
                        match table.get(key)? {
                            Some(row) => vec![row],
                            None => Vec::new(),
                        }
                    }
                    crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                        index_lookups = 1;
                        table.scan(start.as_ref(), end.as_ref())?
                    }
                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                        index_name,
                        scan_type,
                        ..
                    } => {
                        index_lookups = 1;
                        match scan_type {
                            crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                                table.index_lookup(index_name, val)?
                            }
                            crate::query::optimizer::SecondaryScanType::CompositePointLookup(
                                vals,
                            )
                            | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                                table.composite_index_lookup(index_name, vals)?
                            }
                            crate::query::optimizer::SecondaryScanType::RangeScan {
                                start,
                                end,
                            } => table.index_scan(index_name, start.as_ref(), end.as_ref())?,
                        }
                    }
                    crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
                };

                let mut scan_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TableScan {
                        table_name: plan.base_table.table_name.clone(),
                        access_path: plan.base_table.access_path.to_string(),
                    },
                    Some(plan.base_table.estimated_rows),
                    Some(plan.base_table.cost),
                );
                scan_metric.input_rows = initial_rows.len();
                scan_metric.actual_rows = initial_rows.len();
                scan_metric.counters.tuples_fetched = initial_rows.len();
                scan_metric.counters.index_lookups = index_lookups;
                scan_metric.counters.rows_evaluated = initial_rows.len();

                let (filtered_rows, table_op) = match &plan.base_table.residual_filter {
                    Some(filter) => {
                        let filtered = self.filter_rows_with_outer_internal(
                            initial_rows.clone(),
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        filter_metric.input_rows = initial_rows.len();
                        filter_metric.actual_rows = filtered.len();
                        filter_metric.counters.rows_evaluated = initial_rows.len();
                        filter_metric.counters.rows_passed = filtered.len();
                        filter_metric.counters.rows_rejected =
                            initial_rows.len().saturating_sub(filtered.len());
                        filter_metric.add_child(scan_metric);
                        (filtered, filter_metric)
                    }
                    None => (initial_rows, scan_metric),
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

                (schema, filtered_rows, table_op)
            }
        };

        // 2. Execute Joins sequentially
        for join in &plan.joins {
            if join.strategy == crate::query::optimizer::JoinStrategy::IndexProbe {
                if let Some((next_rows, probe_metric)) = self.execute_index_probe_semi_anti(
                    &current_rows,
                    &current_schema,
                    join,
                    &current_op,
                )? {
                    current_rows = next_rows;
                    current_op = probe_metric;
                    continue;
                }
            }
            let (right_schema, right_rows, right_op) = {
                if let Some((cte_schema, cte_rows)) = cte_store.get(&join.right.table_name) {
                    let raw_schema = cte_schema.clone();
                    let initial_rows = cte_rows.clone();
                    let mut scan_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::TableScan {
                            table_name: join.right.table_name.clone(),
                            access_path: "CTE Scan".to_string(),
                        },
                        Some(join.right.estimated_rows),
                        Some(join.right.cost),
                    );
                    scan_metric.input_rows = initial_rows.len();
                    scan_metric.actual_rows = initial_rows.len();
                    scan_metric.counters.tuples_fetched = initial_rows.len();
                    scan_metric.counters.rows_evaluated = initial_rows.len();

                    let (filtered_rows, table_op) = match &join.right.residual_filter {
                        Some(filter) => {
                            let filtered = self.filter_rows_with_outer_internal(
                                initial_rows.clone(),
                                &raw_schema,
                                filter,
                                outer_contexts,
                                cte_store,
                            )?;
                            let mut filter_metric = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Filter,
                                Some(join.right.estimated_rows),
                                Some(join.right.cost),
                            );
                            filter_metric.input_rows = initial_rows.len();
                            filter_metric.actual_rows = filtered.len();
                            filter_metric.counters.rows_evaluated = initial_rows.len();
                            filter_metric.counters.rows_passed = filtered.len();
                            filter_metric.counters.rows_rejected =
                                initial_rows.len().saturating_sub(filtered.len());
                            filter_metric.add_child(scan_metric);
                            (filtered, filter_metric)
                        }
                        None => (initial_rows, scan_metric),
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
                        table_op,
                    )
                } else {
                    let mut table = self.open_table(&join.right.table_name)?;
                    let raw_schema = table.schema().clone();
                    let mut index_lookups = 0;
                    let initial_rows = match &join.right.access_path {
                        crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                            index_lookups = 1;
                            match table.get(key)? {
                                Some(row) => vec![row],
                                None => Vec::new(),
                            }
                        }
                        crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                            index_lookups = 1;
                            table.scan(start.as_ref(), end.as_ref())?
                        }
                        crate::query::optimizer::AccessPath::SecondaryIndexScan {
                            index_name,
                            scan_type,
                            ..
                        } => {
                            index_lookups = 1;
                            match scan_type {
                                crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                                    table.index_lookup(index_name, val)?
                                }
                                crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
                                | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                                    table.composite_index_lookup(index_name, vals)?
                                }
                                crate::query::optimizer::SecondaryScanType::RangeScan {
                                    start,
                                    end,
                                } => table.index_scan(index_name, start.as_ref(), end.as_ref())?,
                            }
                        }
                        crate::query::optimizer::AccessPath::FullTableScan => {
                            table.scan(None, None)?
                        }
                    };

                    let mut scan_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::TableScan {
                            table_name: join.right.table_name.clone(),
                            access_path: join.right.access_path.to_string(),
                        },
                        Some(join.right.estimated_rows),
                        Some(join.right.cost),
                    );
                    scan_metric.input_rows = initial_rows.len();
                    scan_metric.actual_rows = initial_rows.len();
                    scan_metric.counters.tuples_fetched = initial_rows.len();
                    scan_metric.counters.index_lookups = index_lookups;
                    scan_metric.counters.rows_evaluated = initial_rows.len();

                    let (filtered_rows, table_op) = match &join.right.residual_filter {
                        Some(filter) => {
                            let filtered = self.filter_rows_with_outer_internal(
                                initial_rows.clone(),
                                &raw_schema,
                                filter,
                                outer_contexts,
                                cte_store,
                            )?;
                            let mut filter_metric = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Filter,
                                Some(join.right.estimated_rows),
                                Some(join.right.cost),
                            );
                            filter_metric.input_rows = initial_rows.len();
                            filter_metric.actual_rows = filtered.len();
                            filter_metric.counters.rows_evaluated = initial_rows.len();
                            filter_metric.counters.rows_passed = filtered.len();
                            filter_metric.counters.rows_rejected =
                                initial_rows.len().saturating_sub(filtered.len());
                            filter_metric.add_child(scan_metric);
                            (filtered, filter_metric)
                        }
                        None => (initial_rows, scan_metric),
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
                        table_op,
                    )
                }
            };

            let left_count = current_rows.len();
            let right_count = right_rows.len();
            let candidate_pairs = left_count * right_count;

            let (next_schema, next_rows, join_exec_metrics) = if outer_contexts.is_empty() {
                crate::query::operator::JoinOperator::apply_with_config(
                    current_rows,
                    &current_schema,
                    &right_rows,
                    &right_schema,
                    join.join_type,
                    &join.on,
                    &self.db.join_config,
                )?
            } else {
                let (ns, nr) = self.join_rows_with_outer_internal(
                    current_rows,
                    &current_schema,
                    &right_rows,
                    &right_schema,
                    join.join_type,
                    &join.on,
                    outer_contexts,
                    cte_store,
                )?;
                let metrics = crate::query::external_hash_join::JoinExecutionMetrics {
                    rows_left: left_count,
                    rows_right: right_count,
                    rows_output: nr.len(),
                    candidate_pairs,
                    matched_rows: nr.len(),
                    ..Default::default()
                };
                (ns, nr, metrics)
            };

            let op_kind = if join_exec_metrics.runs_created > 0
                || join_exec_metrics.bytes_spilled > 0
            {
                crate::query::OperatorKind::ExternalHashJoin {
                    join_type: format!("{:?}", join.join_type),
                }
            } else if crate::query::external_hash_join::analyze_equi_join(
                &join.on,
                &current_schema,
                &right_schema,
            )
            .is_some()
            {
                match join.join_type {
                    crate::sql::JoinType::Semi => crate::query::OperatorKind::HashSemiJoin,
                    crate::sql::JoinType::Anti => crate::query::OperatorKind::HashAntiJoin,
                    _ => crate::query::OperatorKind::HashJoin {
                        join_type: format!("{:?}", join.join_type),
                    },
                }
            } else {
                match join.join_type {
                    crate::sql::JoinType::Semi => crate::query::OperatorKind::NestedLoopSemiJoin,
                    crate::sql::JoinType::Anti => crate::query::OperatorKind::NestedLoopAntiJoin,
                    _ => crate::query::OperatorKind::NestedLoopJoin {
                        join_type: format!("{:?}", join.join_type),
                    },
                }
            };

            let mut join_metric = crate::query::OperatorMetrics::new(
                op_kind,
                Some(join.estimated_rows),
                Some(join.cost),
            );
            join_metric.input_rows = left_count + right_count;
            join_metric.actual_rows = next_rows.len();
            join_metric.counters.rows_left = left_count;
            join_metric.counters.rows_right = right_count;
            join_metric.counters.candidate_pairs = join_exec_metrics.candidate_pairs;
            join_metric.counters.matched_rows = join_exec_metrics.matched_rows;
            join_metric.counters.matched_outer_rows = join_exec_metrics.matched_outer_rows;
            join_metric.counters.unmatched_outer_rows = join_exec_metrics.unmatched_outer_rows;
            join_metric.counters.hash_build_rows = join_exec_metrics.hash_build_rows;
            join_metric.counters.hash_probe_rows = join_exec_metrics.hash_probe_rows;
            join_metric.counters.rows_output = next_rows.len();
            join_metric.counters.spill_partitions = join_exec_metrics.spill_partitions;
            join_metric.counters.runs_created = join_exec_metrics.runs_created;
            join_metric.counters.bytes_spilled = join_exec_metrics.bytes_spilled;
            join_metric.counters.partition_passes = join_exec_metrics.partition_passes;
            join_metric.counters.peak_memory_bytes = join_exec_metrics.peak_memory_bytes;
            join_metric.add_child(current_op);
            join_metric.add_child(right_op);

            current_schema = next_schema;
            current_rows = next_rows;
            current_op = join_metric;
        }

        // 3. Post-Join Filter
        if let Some(post_filter) = &plan.post_join_filter {
            let pre_count = current_rows.len();
            let filtered_rows = self.filter_rows_with_outer_internal(
                current_rows,
                &current_schema,
                post_filter,
                outer_contexts,
                cte_store,
            )?;
            let mut filter_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Filter,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            filter_metric.input_rows = pre_count;
            filter_metric.actual_rows = filtered_rows.len();
            filter_metric.counters.rows_evaluated = pre_count;
            filter_metric.counters.rows_passed = filtered_rows.len();
            filter_metric.counters.rows_rejected = pre_count.saturating_sub(filtered_rows.len());
            filter_metric.add_child(current_op);
            current_op = filter_metric;
            current_rows = filtered_rows;
        }

        // 4. Aggregation / Group By / Having
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();
        let (mut working_schema, mut working_rows) = if is_aggregate {
            let pre_agg_count = current_rows.len();
            let (grouped_schema, grouped_rows, agg_metrics) =
                crate::query::AggregateOperator::apply_with_config(
                    current_rows,
                    &current_schema,
                    &plan.group_by,
                    &plan.aggregates,
                    &self.db.aggregate_config,
                )?;
            let num_groups = grouped_rows.len();

            let (having_rows, final_rows) = match &plan.having {
                Some(predicate) => {
                    let filtered = self.filter_rows_with_outer_internal(
                        grouped_rows.clone(),
                        &grouped_schema,
                        predicate,
                        outer_contexts,
                        cte_store,
                    )?;
                    (Some(filtered.clone()), filtered)
                }
                None => (None, grouped_rows),
            };

            let mut agg_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Aggregate,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            agg_metric.input_rows = pre_agg_count;
            agg_metric.actual_rows = final_rows.len();
            agg_metric.counters.group_count = agg_metrics.groups_created;
            agg_metric.counters.spill_partitions = agg_metrics.spill_partitions;
            agg_metric.counters.runs_created = agg_metrics.runs_created;
            agg_metric.counters.bytes_spilled = agg_metrics.bytes_spilled;
            agg_metric.counters.merge_passes = agg_metrics.merge_passes;
            agg_metric.counters.peak_memory_bytes = agg_metrics.peak_memory_bytes;
            agg_metric.counters.rows_evaluated = pre_agg_count;
            agg_metric.add_child(current_op);

            if let Some(having_filtered) = having_rows {
                let mut having_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::Filter,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                having_metric.input_rows = num_groups;
                having_metric.actual_rows = having_filtered.len();
                having_metric.counters.rows_evaluated = num_groups;
                having_metric.counters.rows_passed = having_filtered.len();
                having_metric.counters.rows_rejected =
                    num_groups.saturating_sub(having_filtered.len());
                having_metric.add_child(agg_metric);
                current_op = having_metric;
            } else {
                current_op = agg_metric;
            }

            (grouped_schema, final_rows)
        } else {
            (current_schema, current_rows)
        };

        // 5. Window Operator
        if !plan.window_exprs.is_empty() {
            let pre_win_count = working_rows.len();
            let (win_schema, win_rows) = crate::query::WindowOperator::apply(
                working_rows,
                &working_schema,
                &plan.window_exprs,
            )?;
            let mut win_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Window,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            win_metric.input_rows = pre_win_count;
            win_metric.actual_rows = win_rows.len();
            win_metric.add_child(current_op);
            current_op = win_metric;
            working_schema = win_schema;
            working_rows = win_rows;
        }

        // 6. Sort Operator (ORDER BY)
        let sorted_rows = if !plan.order_by.is_empty() {
            let (sorted, sort_metrics) = crate::query::SortOperator::apply_with_config(
                working_rows,
                &working_schema,
                &plan.order_by,
                &self.db.sort_config,
            )?;
            let mut sort_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Sort,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            sort_metric.input_rows = sort_metrics.rows_input;
            sort_metric.actual_rows = sorted.len();
            sort_metric.counters.runs_created = sort_metrics.runs_created;
            sort_metric.counters.bytes_spilled = sort_metrics.bytes_spilled;
            sort_metric.counters.merge_passes = sort_metrics.merge_passes;
            sort_metric.counters.peak_memory_bytes = sort_metrics.peak_memory_bytes;
            sort_metric.add_child(current_op);
            current_op = sort_metric;
            sorted
        } else {
            working_rows
        };

        // 7. Projection
        let pre_proj_count = sorted_rows.len();
        let (projected_schema, projected_rows) = crate::query::ProjectOperator::apply_named(
            sorted_rows,
            &working_schema,
            &plan.projections,
        )?;
        let mut proj_metric = crate::query::OperatorMetrics::new(
            crate::query::OperatorKind::Project,
            Some(plan.estimated_rows),
            Some(plan.total_cost),
        );
        proj_metric.input_rows = pre_proj_count;
        proj_metric.actual_rows = projected_rows.len();
        proj_metric.add_child(current_op);
        current_op = proj_metric;

        // 8. Pagination (OFFSET / LIMIT)
        let paginated_rows = if plan.offset.is_some() || plan.limit.is_some() {
            let pre_limit_count = projected_rows.len();
            let paginated =
                crate::query::OffsetLimitOperator::apply(projected_rows, plan.offset, plan.limit);
            let mut limit_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::LimitOffset,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            limit_metric.input_rows = pre_limit_count;
            limit_metric.actual_rows = paginated.len();
            limit_metric.add_child(current_op);
            current_op = limit_metric;
            paginated
        } else {
            projected_rows
        };

        current_op.execution_time = op_start.elapsed();

        // If CTEs were materialized in this plan, attach them to the top operator
        for cte_metric in cte_metrics {
            current_op.add_child(cte_metric);
        }

        Ok((
            crate::query::QueryResult::new(projected_schema, paginated_rows),
            current_op,
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
        if let Some(set_op) = &mut plan.set_op {
            self.pre_evaluate_uncorrelated_subqueries_internal(
                &mut set_op.left,
                outer_contexts,
                cte_store,
            )?;
            self.pre_evaluate_uncorrelated_subqueries_internal(
                &mut set_op.right,
                outer_contexts,
                cte_store,
            )?;
            return Ok(());
        }
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

    fn execute_index_probe_semi_anti(
        &mut self,
        current_rows: &[Row],
        current_schema: &Schema,
        join: &crate::query::optimizer::PhysicalJoin,
        current_op: &crate::query::OperatorMetrics,
    ) -> Result<Option<(Vec<Row>, crate::query::OperatorMetrics)>> {
        let index_name = match &join.index_name {
            Some(idx) => idx.clone(),
            None => return Ok(None),
        };

        let mut table = self.open_table(&join.right.table_name)?;
        let raw_schema = table.schema().clone();
        let right_qualifier = join.right.qualifier();

        let on_conjuncts = crate::query::optimizer::split_conjunction(&join.on);
        let mut equi_pair: Option<(crate::query::expr::Expr, String)> = None;
        let mut residual_on: Vec<crate::query::expr::Expr> = Vec::new();

        for conj in &on_conjuncts {
            if equi_pair.is_none() {
                if let crate::query::expr::Expr::Binary {
                    left,
                    op: crate::query::expr::BinaryOp::Eq,
                    right,
                } = conj
                {
                    let l_refs = crate::query::optimizer::expr_referenced_qualifiers(left);
                    let r_refs = crate::query::optimizer::expr_referenced_qualifiers(right);

                    if l_refs.len() == 1 && r_refs.len() == 1 {
                        let l_q = l_refs.iter().next().unwrap();
                        let r_q = r_refs.iter().next().unwrap();

                        if l_q == right_qualifier && r_q != right_qualifier {
                            if let crate::query::expr::Expr::Column(inner_col) = left.as_ref() {
                                let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                                equi_pair = Some((*right.clone(), in_name.to_string()));
                                continue;
                            }
                        } else if r_q == right_qualifier && l_q != right_qualifier {
                            if let crate::query::expr::Expr::Column(inner_col) = right.as_ref() {
                                let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                                equi_pair = Some((*left.clone(), in_name.to_string()));
                                continue;
                            }
                        }
                    }
                }
            }
            residual_on.push(conj.clone());
        }

        let (outer_expr, _) = match equi_pair {
            Some(p) => p,
            None => return Ok(None),
        };

        let cols: Vec<crate::engine::types::Column> = raw_schema
            .columns
            .iter()
            .map(|c| {
                crate::engine::types::Column::new(
                    format!("{right_qualifier}.{}", c.name),
                    c.data_type,
                    c.nullable,
                )
            })
            .collect();
        let right_schema = Schema::new_projected(cols);

        let mut combined_cols = current_schema.columns.clone();
        combined_cols.extend(right_schema.columns.clone());
        let combined_schema = Schema::new_projected(combined_cols);

        let is_pk = index_name == "__primary__";
        let is_semi = join.join_type == crate::sql::JoinType::Semi;

        let mut out_rows = Vec::new();
        let mut index_probes = 0;
        let mut successful_probes = 0;
        let mut failed_probes = 0;
        let mut matched_outer_rows = 0;
        let mut unmatched_outer_rows = 0;
        let mut inner_rows_fetched = 0;
        let mut candidate_pairs = 0;

        for outer_row in current_rows {
            let outer_val = match outer_expr.evaluate(outer_row, current_schema)? {
                Value::Null => {
                    unmatched_outer_rows += 1;
                    if !is_semi {
                        out_rows.push(outer_row.clone());
                    }
                    continue;
                }
                v => v,
            };

            index_probes += 1;
            let mut match_found = false;

            if is_pk {
                if let Some(inner_row) = table.get(&outer_val)? {
                    inner_rows_fetched += 1;
                    candidate_pairs += 1;

                    let passes_residual = match &join.right.residual_filter {
                        Some(f) => {
                            let val = f.evaluate(&inner_row, &raw_schema)?;
                            val == Value::Boolean(true)
                        }
                        None => true,
                    };

                    if passes_residual {
                        let passes_on = if residual_on.is_empty() {
                            true
                        } else {
                            let mut combined_values =
                                Vec::with_capacity(outer_row.values.len() + inner_row.values.len());
                            combined_values.extend_from_slice(&outer_row.values);
                            combined_values.extend_from_slice(&inner_row.values);
                            let combined_row = Row::new(combined_values);

                            let mut all_ok = true;
                            for r_expr in &residual_on {
                                if r_expr.evaluate(&combined_row, &combined_schema)?
                                    != Value::Boolean(true)
                                {
                                    all_ok = false;
                                    break;
                                }
                            }
                            all_ok
                        };

                        if passes_on {
                            match_found = true;
                        }
                    }
                }
            } else {
                let rids = table.index_lookup_rids(&index_name, &outer_val)?;
                let has_residuals = join.right.residual_filter.is_some() || !residual_on.is_empty();

                if !has_residuals {
                    if !rids.is_empty() {
                        match_found = true;
                        candidate_pairs += 1;
                    }
                } else {
                    for rid in rids {
                        candidate_pairs += 1;
                        if let Some(inner_row) = table.get_row_by_rid(rid)? {
                            inner_rows_fetched += 1;
                            let passes_residual = match &join.right.residual_filter {
                                Some(f) => {
                                    let val = f.evaluate(&inner_row, &raw_schema)?;
                                    val == Value::Boolean(true)
                                }
                                None => true,
                            };

                            if passes_residual {
                                let passes_on = if residual_on.is_empty() {
                                    true
                                } else {
                                    let mut combined_values = Vec::with_capacity(
                                        outer_row.values.len() + inner_row.values.len(),
                                    );
                                    combined_values.extend_from_slice(&outer_row.values);
                                    combined_values.extend_from_slice(&inner_row.values);
                                    let combined_row = Row::new(combined_values);

                                    let mut all_ok = true;
                                    for r_expr in &residual_on {
                                        if r_expr.evaluate(&combined_row, &combined_schema)?
                                            != Value::Boolean(true)
                                        {
                                            all_ok = false;
                                            break;
                                        }
                                    }
                                    all_ok
                                };

                                if passes_on {
                                    match_found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if match_found {
                successful_probes += 1;
                matched_outer_rows += 1;
                if is_semi {
                    out_rows.push(outer_row.clone());
                }
            } else {
                failed_probes += 1;
                unmatched_outer_rows += 1;
                if !is_semi {
                    out_rows.push(outer_row.clone());
                }
            }
        }

        let op_kind = if is_semi {
            crate::query::OperatorKind::IndexSemiProbe {
                index_name: index_name.clone(),
            }
        } else {
            crate::query::OperatorKind::IndexAntiProbe {
                index_name: index_name.clone(),
            }
        };

        let mut join_metric =
            crate::query::OperatorMetrics::new(op_kind, Some(join.estimated_rows), Some(join.cost));
        join_metric.input_rows = current_rows.len() + inner_rows_fetched;
        join_metric.actual_rows = out_rows.len();
        join_metric.counters.rows_left = current_rows.len();
        join_metric.counters.rows_right = inner_rows_fetched;
        join_metric.counters.matched_outer_rows = matched_outer_rows;
        join_metric.counters.unmatched_outer_rows = unmatched_outer_rows;
        join_metric.counters.index_probes = index_probes;
        join_metric.counters.successful_probes = successful_probes;
        join_metric.counters.failed_probes = failed_probes;
        join_metric.counters.candidate_pairs = candidate_pairs;
        join_metric.counters.rows_output = out_rows.len();

        let mut inner_scan_metric = crate::query::OperatorMetrics::new(
            crate::query::OperatorKind::TableScan {
                table_name: join.right.table_name.clone(),
                access_path: format!("IndexProbe({index_name})"),
            },
            Some(join.right.estimated_rows),
            Some(join.right.cost),
        );
        inner_scan_metric.input_rows = inner_rows_fetched;
        inner_scan_metric.actual_rows = inner_rows_fetched;
        inner_scan_metric.counters.index_lookups = index_probes;
        inner_scan_metric.counters.tuples_fetched = inner_rows_fetched;

        join_metric.add_child(current_op.clone());
        join_metric.add_child(inner_scan_metric);

        Ok(Some((out_rows, join_metric)))
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
            | crate::query::expr::Expr::OuterColumn { .. }
            | crate::query::expr::Expr::Parameter { .. } => {}
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
            | crate::query::expr::Expr::OuterColumn { .. }
            | crate::query::expr::Expr::Parameter { .. } => {
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
        if outer_contexts.is_empty() {
            let (s, r, _) = crate::query::operator::JoinOperator::apply_with_config(
                left_rows,
                left_schema,
                right_rows,
                right_schema,
                join_type,
                on,
                &self.db.join_config,
            )?;
            return Ok((s, r));
        }
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
        let mut combined_columns =
            Vec::with_capacity(left_schema.columns.len() + right_schema.columns.len());
        for col in &left_schema.columns {
            let mut left_col = col.clone();
            if join_type == crate::sql::ast::JoinType::Right
                || join_type == crate::sql::ast::JoinType::Full
            {
                left_col.nullable = true;
            }
            combined_columns.push(left_col);
        }
        for col in &right_schema.columns {
            let mut right_col = col.clone();
            if join_type == crate::sql::ast::JoinType::Left
                || join_type == crate::sql::ast::JoinType::Full
            {
                right_col.nullable = true;
            }
            combined_columns.push(right_col);
        }
        let combined_schema = Schema::new_projected(combined_columns);

        let left_null_values = vec![Value::Null; left_schema.columns.len()];
        let right_null_values = vec![Value::Null; right_schema.columns.len()];
        let mut result_rows = Vec::new();
        let mut right_matched = vec![false; right_rows.len()];

        for l_row in left_rows {
            let mut left_matched = false;
            for (r_idx, r_row) in right_rows.iter().enumerate() {
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
                    left_matched = true;
                    right_matched[r_idx] = true;
                    result_rows.push(candidate_row);
                }
            }
            if !left_matched
                && (join_type == crate::sql::ast::JoinType::Left
                    || join_type == crate::sql::ast::JoinType::Full)
            {
                let mut combined_vals =
                    Vec::with_capacity(l_row.values.len() + right_null_values.len());
                combined_vals.extend_from_slice(&l_row.values);
                combined_vals.extend_from_slice(&right_null_values);
                result_rows.push(Row::new(combined_vals));
            }
        }

        if join_type == crate::sql::ast::JoinType::Right
            || join_type == crate::sql::ast::JoinType::Full
        {
            for (r_idx, r_row) in right_rows.iter().enumerate() {
                if !right_matched[r_idx] {
                    let mut combined_vals =
                        Vec::with_capacity(left_null_values.len() + r_row.values.len());
                    combined_vals.extend_from_slice(&left_null_values);
                    combined_vals.extend_from_slice(&r_row.values);
                    result_rows.push(Row::new(combined_vals));
                }
            }
        }

        Ok((combined_schema, result_rows))
    }

    /// Drops a table from the catalog and reclaims its pages within this transaction.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::drop_table(&mut self.db.buffer_pool, name);
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
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

    /// Returns diagnostic plan cache statistics.
    pub fn plan_cache_stats(&self) -> crate::query::plan_cache::PlanCacheStats {
        self.db.plan_cache_stats()
    }

    /// Clears all entries from the physical plan cache.
    pub fn clear_plan_cache(&mut self) {
        self.db.clear_plan_cache();
    }

    /// Commits all mutations performed in this transaction, flushing the WAL through the commit LSN.
    pub fn commit(mut self) -> Result<()> {
        self.ensure_active()?;
        self.db.commit_txn(self.txn_id)?;
        self.state = TxnState::Committed;
        if self.has_catalog_mutations {
            self.db.catalog_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
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
            if self.has_catalog_mutations {
                self.db.catalog_generation += 1;
                self.db.index_generation += 1;
                self.db.plan_cache.invalidate_all();
            }
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
