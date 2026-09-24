use crate::engine::TreeDb;
use crate::error::{Error, Result};
use crate::storage::page::{Key, PageId};
use crate::wal::lsn::TxnId;

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

    /// Parses, binds, and executes an SQL `SELECT` statement within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_sql(&mut self, sql: &str) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;
        let ast = crate::sql::parse(sql)?;
        let mut table = self.open_table(&ast.from.name)?;

        if ast.joins.is_empty() {
            let bound = crate::sql::bind_select(&ast, table.schema())?;

            let mut query = crate::query::Query::new(table);
            if let Some(filter) = bound.filter {
                query = query.filter(filter);
            }
            if !bound.group_by.is_empty() {
                query = query.group_by(bound.group_by);
            }
            if !bound.aggregates.is_empty() {
                query = query.aggregate(bound.aggregates);
            }
            if let Some(having) = bound.having {
                query = query.having(having);
            }
            if !bound.projections.is_empty() {
                query = query.project_with_aliases(bound.projections);
            }
            if !bound.order_by.is_empty() {
                query = query.sort(bound.order_by);
            }
            if let Some(offset) = bound.offset {
                query = query.offset(offset);
            }
            if let Some(limit) = bound.limit {
                query = query.limit(limit);
            }

            query.execute()
        } else {
            // Multi-table JOIN query execution
            let (from_schema, mut current_rows) = {
                let schema = table.schema().clone();
                let rows = table.scan(None, None)?;
                (schema, rows)
            };

            let mut joined_info = Vec::new();
            for j in &ast.joins {
                let mut t = self.open_table(&j.table.name)?;
                let schema = t.schema().clone();
                let rows = t.scan(None, None)?;
                joined_info.push((j, schema, rows));
            }

            let mut bindings = Vec::new();
            bindings.push(crate::sql::TableBinding::new(
                &ast.from.name,
                ast.from.alias.as_deref(),
                &from_schema,
            ));
            for (j, schema, _) in &joined_info {
                bindings.push(crate::sql::TableBinding::new(
                    &j.table.name,
                    j.table.alias.as_deref(),
                    schema,
                ));
            }

            let bound = crate::sql::bind_select_multi(&ast, &bindings)?;

            // 1. Qualified schema for left-most table
            let mut current_schema = {
                let q = bindings[0].qualifier();
                let cols: Vec<crate::engine::types::Column> = bindings[0]
                    .schema
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

            // 2. Perform chained joins left-to-right
            for (join_idx, (j, right_schema_raw, right_rows)) in joined_info.iter().enumerate() {
                let right_schema = {
                    let q = bindings[join_idx + 1].qualifier();
                    let cols: Vec<crate::engine::types::Column> = right_schema_raw
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

                let (next_schema, next_rows) = crate::query::JoinOperator::apply(
                    current_rows,
                    &current_schema,
                    right_rows,
                    &right_schema,
                    j.join_type,
                    &bound.joins[join_idx].on,
                )?;
                current_schema = next_schema;
                current_rows = next_rows;
            }

            // 3. Filter Operator (WHERE)
            let filtered_rows = match &bound.filter {
                Some(predicate) => {
                    crate::query::FilterOperator::apply(current_rows, &current_schema, predicate)?
                }
                None => current_rows,
            };

            // 4. Aggregation & HAVING or standard Sort & Project
            let is_aggregate = !bound.group_by.is_empty() || !bound.aggregates.is_empty();
            if is_aggregate {
                let (grouped_schema, grouped_rows) = crate::query::AggregateOperator::apply(
                    filtered_rows,
                    &current_schema,
                    &bound.group_by,
                    &bound.aggregates,
                )?;

                let having_rows = match &bound.having {
                    Some(predicate) => crate::query::FilterOperator::apply(
                        grouped_rows,
                        &grouped_schema,
                        predicate,
                    )?,
                    None => grouped_rows,
                };

                let sorted_rows = crate::query::SortOperator::apply(
                    having_rows,
                    &grouped_schema,
                    &bound.order_by,
                )?;

                let (projected_schema, projected_rows) =
                    crate::query::ProjectOperator::apply_named(
                        sorted_rows,
                        &grouped_schema,
                        &bound.projections,
                    )?;
                let paginated_rows = crate::query::OffsetLimitOperator::apply(
                    projected_rows,
                    bound.offset,
                    bound.limit,
                );
                Ok(crate::query::QueryResult::new(
                    projected_schema,
                    paginated_rows,
                ))
            } else {
                let sorted_rows = crate::query::SortOperator::apply(
                    filtered_rows,
                    &current_schema,
                    &bound.order_by,
                )?;

                let (projected_schema, projected_rows) =
                    crate::query::ProjectOperator::apply_named(
                        sorted_rows,
                        &current_schema,
                        &bound.projections,
                    )?;
                let paginated_rows = crate::query::OffsetLimitOperator::apply(
                    projected_rows,
                    bound.offset,
                    bound.limit,
                );
                Ok(crate::query::QueryResult::new(
                    projected_schema,
                    paginated_rows,
                ))
            }
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
