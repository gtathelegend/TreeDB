pub mod catalog;
pub mod table;
pub mod transaction;
pub mod types;

pub use crate::query::external_multi_index::{MultiIndexConfig, MultiIndexExecutionMode};
pub use crate::query::materialize::{MaterializeConfig, MaterializeOverride};
pub use catalog::{
    Catalog, CatalogPage, IndexId, IndexMetadata, TableId, TableMetadata, CATALOG_MAGIC,
    CURRENT_CATALOG_VERSION, MAX_INDEX_NAME_LEN, MAX_TABLE_NAME_LEN,
};
pub use table::{MultiIndexScanStats, SkipScanStats, Table};
pub use transaction::{StatementResult, Transaction, TxnState};
pub use types::{Column, DataType, Row, Schema, Value, MAX_COLUMN_NAME_LEN, TUPLE_FORMAT_VERSION};

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::query::external_aggregate::AggregateConfig;
use crate::query::external_hash_join::JoinConfig;
use crate::query::external_sort::SortConfig;
use crate::query::feedback::FeedbackRegistry;
use crate::query::plan_cache::{PlanCache, PlanCacheStats};
use crate::query::stats::{StatsRegistry, TableStatistics};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{Key, PageId};
use crate::storage::page_manager::PageManager;
use crate::storage::record_store::RecordStore;
use crate::wal::log_manager::LogManager;
use crate::wal::lsn::{Lsn, TxnId, NULL_LSN, NULL_TXN};
use crate::wal::recovery::RecoveryEngine;

/// High-level persistent database engine coordinating the B+ Tree index,
/// slotted Record Store, Buffer Pool, and Write-Ahead Logging.
#[derive(Debug)]
pub struct TreeDb {
    tree: BPlusTree,
    record_store: RecordStore,
    buffer_pool: BufferPool,
    active_txn: Option<TxnId>,
    stats: StatsRegistry,
    pub(crate) catalog_generation: u64,
    pub(crate) index_generation: u64,
    pub(crate) statistics_generation: u64,
    pub(crate) plan_cache: PlanCache,
    pub(crate) feedback_registry: FeedbackRegistry,
    pub(crate) sort_config: SortConfig,
    pub(crate) aggregate_config: AggregateConfig,
    pub(crate) join_config: JoinConfig,
    pub(crate) multi_index_config: MultiIndexConfig,
    pub(crate) materialize_config: MaterializeConfig,
}

impl TreeDb {
    /// Opens an existing TreeDB database file, recovering any pending WAL records if present.
    pub fn open(path: impl AsRef<Path>, pool_capacity: usize) -> Result<Self> {
        let db_path = path.as_ref();
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));

        let mut page_manager = PageManager::open(db_path)?;
        let log_manager = if wal_path.exists() {
            let mut log_mgr = LogManager::open(&wal_path)?;
            // Execute ARIES crash recovery before initializing buffer pool
            RecoveryEngine::recover(&mut page_manager, &mut log_mgr)?;
            log_mgr
        } else {
            LogManager::create(&wal_path)?
        };

        let root_page_id = page_manager.header().root_page_id;
        let buffer_pool = BufferPool::with_wal(pool_capacity, page_manager, log_manager)?;
        let tree = BPlusTree::from_root(root_page_id);
        let record_store = RecordStore::new();

        Ok(Self {
            tree,
            record_store,
            buffer_pool,
            active_txn: None,
            stats: StatsRegistry::new(),
            catalog_generation: 1,
            index_generation: 1,
            statistics_generation: 1,
            plan_cache: PlanCache::new(),
            feedback_registry: FeedbackRegistry::new(),
            sort_config: SortConfig::default(),
            aggregate_config: AggregateConfig::default(),
            join_config: JoinConfig::default(),
            multi_index_config: MultiIndexConfig::default(),
            materialize_config: crate::query::materialize::MaterializeConfig::default(),
        })
    }

    /// Creates and initializes a new TreeDB database file with an associated WAL.
    pub fn create(path: impl AsRef<Path>, pool_capacity: usize) -> Result<Self> {
        let db_path = path.as_ref();
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));

        let page_manager = PageManager::create(db_path)?;
        let log_manager = LogManager::create(&wal_path)?;
        let root_page_id = page_manager.header().root_page_id;
        let buffer_pool = BufferPool::with_wal(pool_capacity, page_manager, log_manager)?;
        let tree = BPlusTree::from_root(root_page_id);
        let record_store = RecordStore::new();

        Ok(Self {
            tree,
            record_store,
            buffer_pool,
            active_txn: None,
            stats: StatsRegistry::new(),
            catalog_generation: 1,
            index_generation: 1,
            statistics_generation: 1,
            plan_cache: PlanCache::new(),
            feedback_registry: FeedbackRegistry::new(),
            sort_config: SortConfig::default(),
            aggregate_config: AggregateConfig::default(),
            join_config: JoinConfig::default(),
            multi_index_config: MultiIndexConfig::default(),
            materialize_config: crate::query::materialize::MaterializeConfig::default(),
        })
    }

    /// Creates a `TreeDb` instance from an existing `BufferPool` and root `PageId`.
    pub fn from_components(
        tree: BPlusTree,
        record_store: RecordStore,
        buffer_pool: BufferPool,
    ) -> Self {
        Self {
            tree,
            record_store,
            buffer_pool,
            active_txn: None,
            stats: StatsRegistry::new(),
            catalog_generation: 1,
            index_generation: 1,
            statistics_generation: 1,
            plan_cache: PlanCache::new(),
            feedback_registry: FeedbackRegistry::new(),
            sort_config: SortConfig::default(),
            aggregate_config: AggregateConfig::default(),
            join_config: JoinConfig::default(),
            multi_index_config: MultiIndexConfig::default(),
            materialize_config: crate::query::materialize::MaterializeConfig::default(),
        }
    }

    /// Begins an explicit atomic transaction.
    ///
    /// # Errors
    /// * `Error::TransactionActive` if a transaction is already active on this database.
    pub fn begin(&mut self) -> Result<Transaction<'_>> {
        if let Some(active) = self.active_txn {
            return Err(Error::TransactionActive(active));
        }

        let txn_id = self.begin_txn()?;
        let header = self.buffer_pool.page_manager().header().clone();

        Ok(Transaction::new(
            self,
            txn_id,
            header.root_page_id,
            header.freelist_head,
            header.free_page_count,
            header.page_count,
        ))
    }

    /// Convenience alias for `begin()`.
    pub fn begin_transaction(&mut self) -> Result<Transaction<'_>> {
        self.begin()
    }

    /// Begins a new transaction in the WAL stream.
    pub fn begin_txn(&mut self) -> Result<TxnId> {
        if let Some(wal) = self.buffer_pool.wal_mut() {
            let txn_id = wal.begin_txn()?;
            self.active_txn = Some(txn_id);
            self.buffer_pool.set_active_txn(txn_id);
            Ok(txn_id)
        } else {
            Ok(NULL_TXN)
        }
    }

    /// Commits the specified transaction, flushing the WAL through the commit LSN.
    pub fn commit_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        if let Some(wal) = self.buffer_pool.wal_mut() {
            let lsn = wal.commit_txn(txn_id)?;
            if self.active_txn == Some(txn_id) {
                self.active_txn = None;
                self.buffer_pool.clear_active_txn();
            }
            Ok(lsn)
        } else {
            Ok(NULL_LSN)
        }
    }

    /// Aborts the specified transaction, rolling back physical page modifications
    /// using Compensation Log Records (CLRs) and flushing WAL through the abort LSN.
    pub fn abort_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        let lsn = self.buffer_pool.abort_txn(txn_id)?;
        if self.active_txn == Some(txn_id) {
            self.active_txn = None;
            self.buffer_pool.clear_active_txn();
        }
        Ok(lsn)
    }

    /// Appends a `PageDelta` log record and updates the target page's `page_lsn`.
    pub fn log_page_delta(
        &mut self,
        txn_id: TxnId,
        page_id: PageId,
        offset: u16,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    ) -> Result<Lsn> {
        if let Some(wal) = self.buffer_pool.wal_mut() {
            let lsn = wal.append_page_delta(txn_id, page_id, offset, before_image, after_image)?;
            let page = self.buffer_pool.get_page_mut(page_id)?;
            page.set_lsn(lsn);
            Ok(lsn)
        } else {
            Ok(NULL_LSN)
        }
    }

    /// Inserts or updates a key-value mapping in the database.
    ///
    /// Stores the variable-length value payload in the Record Store and updates the B+ Tree index.
    pub fn put(&mut self, key: Key, value: &[u8]) -> Result<()> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        if let Some(old_rid) = self.tree.get(&mut self.buffer_pool, key)? {
            let record_id = self.record_store.insert(&mut self.buffer_pool, value)?;
            self.tree.update(&mut self.buffer_pool, key, record_id)?;
            self.record_store.delete(&mut self.buffer_pool, old_rid)?;
        } else {
            let record_id = self.record_store.insert(&mut self.buffer_pool, value)?;
            self.tree.insert(&mut self.buffer_pool, key, record_id)?;
        }

        if let Some(tid) = auto_txn {
            self.commit_txn(tid)?;
        }

        Ok(())
    }

    /// Updates an existing key-value mapping in the database.
    pub fn update(&mut self, key: Key, value: &[u8]) -> Result<()> {
        self.put(key, value)
    }

    /// Retrieves an owned copy of the value bytes associated with `key`.
    ///
    /// Returns `Ok(Some(Vec<u8>))` if the key exists, or `Ok(None)` if absent.
    pub fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        match self.tree.get(&mut self.buffer_pool, key)? {
            Some(record_id) => self.record_store.get(&mut self.buffer_pool, record_id),
            None => Ok(None),
        }
    }

    /// Deletes the key and its associated record from the database.
    ///
    /// Returns `Ok(Some(Vec<u8>))` with the deleted record bytes if present, or `Ok(None)` if absent.
    pub fn delete(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        let result = match self.tree.delete(&mut self.buffer_pool, key)? {
            Some(record_id) => {
                let value = self.record_store.get(&mut self.buffer_pool, record_id)?;
                self.record_store.delete(&mut self.buffer_pool, record_id)?;
                Ok(value)
            }
            None => Ok(None),
        };

        if let Some(tid) = auto_txn {
            self.commit_txn(tid)?;
        }

        result
    }

    /// Performs an ordered range scan over keys in `[start, end)`, returning pairs of `(Key, Vec<u8>)`.
    pub fn scan(&mut self, start: Option<Key>, end: Option<Key>) -> Result<Vec<(Key, Vec<u8>)>> {
        let rids = self.tree.scan(&mut self.buffer_pool, start, end)?;
        let mut results = Vec::with_capacity(rids.len());

        for (k, rid) in rids {
            if let Some(val) = self.record_store.get(&mut self.buffer_pool, rid)? {
                results.push((k, val));
            }
        }

        Ok(results)
    }

    /// Synchronizes all dirty buffer pool frames and flushes file changes to physical storage.
    pub fn sync(&mut self) -> Result<()> {
        self.buffer_pool.sync()
    }

    /// Executes a synchronous checkpoint of the database.
    ///
    /// 1. Flushes all dirty buffer pool pages to disk.
    /// 2. Records active uncommitted transactions.
    /// 3. Appends a Checkpoint record to WAL and flushes WAL.
    /// 4. Updates Page 0 DbHeader with last_checkpoint_lsn and syncs database file.
    pub fn checkpoint(&mut self) -> Result<Lsn> {
        self.buffer_pool.checkpoint()
    }

    /// Executes a synchronous checkpoint and safely truncates obsolete WAL records.
    pub fn checkpoint_and_truncate_wal(&mut self) -> Result<Lsn> {
        self.buffer_pool.checkpoint_and_truncate_wal()
    }

    /// Returns a reference to the underlying B+ Tree handle.
    pub const fn tree(&self) -> &BPlusTree {
        &self.tree
    }

    /// Returns a reference to the underlying RecordStore.
    pub const fn record_store(&self) -> &RecordStore {
        &self.record_store
    }

    /// Returns a reference to the underlying BufferPool.
    pub const fn buffer_pool(&self) -> &BufferPool {
        &self.buffer_pool
    }

    /// Returns a mutable reference to the underlying BufferPool.
    pub fn buffer_pool_mut(&mut self) -> &mut BufferPool {
        &mut self.buffer_pool
    }

    /// Validates the structural invariants of the B+ Tree.
    pub fn validate_tree(&mut self) -> Result<()> {
        self.tree.validate_tree(&mut self.buffer_pool)
    }

    /// Creates a new table in the catalog.
    ///
    /// Automatically begins and commits a transaction if no transaction is active.
    pub fn create_table(&mut self, name: &str, schema: Schema) -> Result<TableId> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        let result = Catalog::create_table(&mut self.buffer_pool, name, schema);

        if result.is_ok() {
            self.catalog_generation += 1;
            self.plan_cache.invalidate_all();
        }

        if let Some(tid) = auto_txn {
            if result.is_ok() {
                self.commit_txn(tid)?;
            } else {
                let _ = self.abort_txn(tid);
            }
        }

        result
    }

    /// Opens an existing table handle for relational operations.
    pub fn open_table(&mut self, name: &str) -> Result<Table<'_>> {
        let metadata = self
            .get_table(name)?
            .ok_or_else(|| Error::TableNotFound(name.to_string()))?;
        let tree = BPlusTree::from_root(metadata.root_page_id);
        let record_store = RecordStore::with_last_page(metadata.record_page_id);
        let index_metas =
            Catalog::list_indexes_for_table(&mut self.buffer_pool, metadata.table_id)?;
        let mut indexes = Vec::with_capacity(index_metas.len());
        for idx_meta in index_metas {
            let sec_tree =
                crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
            indexes.push((idx_meta, sec_tree));
        }

        Ok(Table::new(
            metadata,
            tree,
            record_store,
            indexes,
            &mut self.buffer_pool,
        ))
    }

    /// Constructs a programmatic query against a named table.
    pub fn query(&mut self, table_name: &str) -> Result<crate::query::Query<'_>> {
        let table = self.open_table(table_name)?;
        Ok(crate::query::Query::new(table))
    }

    /// Parses, binds, and produces an optimized physical execution plan (`PhysicalPlan`) for the given SQL query.
    pub fn plan_sql(&mut self, sql: &str) -> Result<crate::query::optimizer::PhysicalPlan> {
        let mut txn = self.begin_transaction()?;
        let plan = txn.plan_sql(sql)?;
        txn.commit()?;
        Ok(plan)
    }

    /// Generates a human-readable text explanation of the optimized physical execution plan for the given SQL query.
    pub fn explain_sql(&mut self, sql: &str) -> Result<String> {
        let mut txn = self.begin_transaction()?;
        let explanation = txn.explain_sql(sql)?;
        txn.commit()?;
        Ok(explanation)
    }

    /// Generates a human-readable explanation including the deterministic physical plan fingerprint for a SQL query.
    pub fn explain_sql_with_fingerprint(&mut self, sql: &str) -> Result<String> {
        let mut txn = self.begin_transaction()?;
        let explanation = txn.explain_sql_with_fingerprint(sql)?;
        txn.commit()?;
        Ok(explanation)
    }

    /// Computes the deterministic physical plan fingerprint for a SQL query within an auto-committed transaction.
    pub fn explain_plan_fingerprint(
        &mut self,
        sql: &str,
    ) -> Result<crate::query::fingerprint::PhysicalPlanFingerprint> {
        let mut txn = self.begin_transaction()?;
        let fp = txn.explain_plan_fingerprint(sql)?;
        txn.commit()?;
        Ok(fp)
    }

    /// Executes an arbitrary top-level SQL statement (`SELECT`, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`)
    /// directly on the database engine within an auto-committed transaction.
    pub fn execute_statement(&mut self, sql: &str) -> Result<StatementResult> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_statement(sql)?;
        txn.commit()?;
        Ok(result)
    }

    /// Parses, binds, optimizes, and executes an SQL `SELECT` statement directly on the database engine.
    pub fn execute_sql(&mut self, sql: &str) -> Result<crate::query::QueryResult> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_sql(sql)?;
        txn.commit()?;
        Ok(result)
    }

    /// Parses, binds, optimizes, and executes an SQL statement directly on the database engine,
    /// returning both the query result and runtime query execution metrics within an auto-committed transaction.
    pub fn execute_sql_with_metrics(
        &mut self,
        sql: &str,
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_sql_with_metrics(sql)?;
        txn.commit()?;
        Ok(result)
    }

    /// Prepares an SQL query string into a reusable `PreparedStatement` with inferred parameter metadata.
    pub fn prepare_sql(&mut self, sql: &str) -> Result<crate::sql::PreparedStatement> {
        let mut txn = self.begin_transaction()?;
        let result = txn.prepare_sql(sql)?;
        txn.commit()?;
        Ok(result)
    }

    /// Executes a reusable `PreparedStatement` with parameter values directly on the database engine within an auto-committed transaction.
    pub fn execute_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<StatementResult> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_prepared(stmt, params)?;
        txn.commit()?;
        Ok(result)
    }

    /// Executes a prepared query statement directly on the database engine within an auto-committed transaction.
    pub fn execute_prepared_query(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<crate::query::QueryResult> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_prepared_query(stmt, params)?;
        txn.commit()?;
        Ok(result)
    }

    /// Executes a prepared statement returning both the query result and runtime metrics within an auto-committed transaction.
    pub fn execute_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        let mut txn = self.begin_transaction()?;
        let result = txn.execute_prepared_with_metrics(stmt, params)?;
        txn.commit()?;
        Ok(result)
    }

    /// Explains the physical execution plan for a prepared statement with concrete parameters within an auto-committed transaction.
    pub fn explain_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        let mut txn = self.begin_transaction()?;
        let result = txn.explain_prepared(stmt, params)?;
        txn.commit()?;
        Ok(result)
    }

    /// Explains and analyzes the physical execution plan for a prepared statement with concrete parameters within an auto-committed transaction.
    pub fn explain_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        let mut txn = self.begin_transaction()?;
        let result = txn.explain_prepared_with_metrics(stmt, params)?;
        txn.commit()?;
        Ok(result)
    }

    /// Creates a persistent secondary index.
    pub fn create_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_name: &str,
        unique: bool,
    ) -> Result<IndexId> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        let result = Catalog::create_index(
            &mut self.buffer_pool,
            table_name,
            index_name,
            column_name,
            unique,
        );

        if result.is_ok() {
            self.catalog_generation += 1;
            self.index_generation += 1;
            self.plan_cache.invalidate_all();
            self.feedback_registry.clear_all();
        }

        if let Some(tid) = auto_txn {
            if result.is_ok() {
                self.commit_txn(tid)?;
            } else {
                let _ = self.abort_txn(tid);
            }
        }

        result
    }

    /// Drops a secondary index from the catalog and reclaims its pages.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        let result = Catalog::drop_index(&mut self.buffer_pool, name);

        if result.is_ok() {
            self.catalog_generation += 1;
            self.index_generation += 1;
            self.plan_cache.invalidate_all();
            self.feedback_registry.clear_all();
        }

        if let Some(tid) = auto_txn {
            if result.is_ok() {
                self.commit_txn(tid)?;
            } else {
                let _ = self.abort_txn(tid);
            }
        }

        result
    }

    /// Looks up a secondary index by name.
    pub fn get_index(&mut self, name: &str) -> Result<Option<IndexMetadata>> {
        Catalog::get_index(&mut self.buffer_pool, name)
    }

    /// Lists all secondary indexes in the database.
    pub fn list_indexes(&mut self) -> Result<Vec<IndexMetadata>> {
        Catalog::list_indexes(&mut self.buffer_pool)
    }

    /// Drops a table from the catalog and reclaims all of its physical pages into the freelist.
    ///
    /// Automatically begins and commits a transaction if no transaction is active.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let auto_txn = if self.active_txn.is_none() && self.buffer_pool.wal().is_some() {
            Some(self.begin_txn()?)
        } else {
            None
        };

        let result = Catalog::drop_table(&mut self.buffer_pool, name);

        if result.is_ok() {
            self.catalog_generation += 1;
            self.plan_cache.invalidate_all();
            self.feedback_registry.clear_table_feedback(name);
        }

        if let Some(tid) = auto_txn {
            if result.is_ok() {
                self.commit_txn(tid)?;
            } else {
                let _ = self.abort_txn(tid);
            }
        }

        result
    }

    /// Looks up a table in the catalog by name.
    pub fn get_table(&mut self, name: &str) -> Result<Option<TableMetadata>> {
        Catalog::get_table(&mut self.buffer_pool, name)
    }

    /// Looks up a table in the catalog by table ID.
    pub fn get_table_by_id(&mut self, table_id: TableId) -> Result<Option<TableMetadata>> {
        Catalog::get_table_by_id(&mut self.buffer_pool, table_id)
    }

    /// Lists all tables registered in the catalog.
    pub fn list_tables(&mut self) -> Result<Vec<TableMetadata>> {
        Catalog::list_tables(&mut self.buffer_pool)
    }

    /// Validates the structural and referential integrity of the catalog.
    pub fn validate_catalog(&mut self) -> Result<()> {
        Catalog::validate_catalog(&mut self.buffer_pool)
    }

    /// Derives and registers runtime statistics for the named table.
    pub fn collect_statistics(&mut self, table_name: &str) -> Result<TableStatistics> {
        let mut table = self.open_table(table_name)?;
        let stats = table.collect_statistics()?;
        self.stats.set_table_stats(stats.clone());
        self.statistics_generation += 1;
        self.plan_cache.invalidate_statistics(table_name);
        self.feedback_registry
            .invalidate_generation(table_name, self.statistics_generation);
        Ok(stats)
    }

    /// Retrieves registered statistics for a table if available.
    pub fn get_statistics(&self, table_name: &str) -> Option<&TableStatistics> {
        self.stats.get_table_stats(table_name)
    }

    /// Sets or overrides statistics for a table.
    pub fn set_statistics(&mut self, stats: TableStatistics) {
        let table_name = stats.table_name.clone();
        self.stats.set_table_stats(stats);
        self.statistics_generation += 1;
        self.plan_cache.invalidate_statistics(&table_name);
        self.feedback_registry
            .invalidate_generation(&table_name, self.statistics_generation);
    }

    /// Removes registered statistics for a table.
    pub fn clear_statistics(&mut self, table_name: &str) {
        self.stats.remove_table_stats(table_name);
        self.statistics_generation += 1;
        self.plan_cache.invalidate_statistics(table_name);
        self.feedback_registry
            .invalidate_generation(table_name, self.statistics_generation);
    }

    /// Returns a reference to the database statistics registry.
    pub fn statistics_registry(&self) -> &StatsRegistry {
        &self.stats
    }

    /// Returns a mutable reference to the database statistics registry.
    pub fn statistics_registry_mut(&mut self) -> &mut StatsRegistry {
        &mut self.stats
    }

    /// Returns a reference to the adaptive runtime cardinality feedback registry.
    pub fn feedback_registry(&self) -> &FeedbackRegistry {
        &self.feedback_registry
    }

    /// Returns a mutable reference to the adaptive runtime cardinality feedback registry.
    pub fn feedback_registry_mut(&mut self) -> &mut FeedbackRegistry {
        &mut self.feedback_registry
    }

    /// Clears all recorded runtime feedback entries.
    pub fn clear_feedback(&mut self) {
        self.feedback_registry.clear_all();
    }

    /// Clears feedback entries associated with a specific table.
    pub fn clear_table_feedback(&mut self, table_name: &str) {
        self.feedback_registry.clear_table_feedback(table_name);
    }

    /// Invalidates feedback entries whose statistics generation no longer matches.
    pub fn invalidate_feedback_generation(&mut self, table_name: &str, current_stats_gen: u64) {
        self.feedback_registry
            .invalidate_generation(table_name, current_stats_gen);
    }

    /// Returns a reference to the physical plan cache.
    pub fn plan_cache(&self) -> &PlanCache {
        &self.plan_cache
    }

    /// Returns a mutable reference to the physical plan cache.
    pub fn plan_cache_mut(&mut self) -> &mut PlanCache {
        &mut self.plan_cache
    }

    /// Returns diagnostic plan cache statistics.
    pub fn plan_cache_stats(&self) -> PlanCacheStats {
        self.plan_cache.stats()
    }

    /// Clears all entries from the physical plan cache.
    pub fn clear_plan_cache(&mut self) {
        self.plan_cache.clear();
    }

    /// Sets the maximum capacity of the physical plan cache.
    pub fn set_plan_cache_capacity(&mut self, capacity: usize) {
        self.plan_cache.set_capacity(capacity);
    }

    /// Returns the current catalog/schema generation counter.
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Returns the current index generation counter.
    pub fn index_generation(&self) -> u64 {
        self.index_generation
    }

    /// Returns the current statistics generation counter.
    pub fn statistics_generation(&self) -> u64 {
        self.statistics_generation
    }

    /// Returns a reference to the active external sort configuration.
    pub fn sort_config(&self) -> &SortConfig {
        &self.sort_config
    }

    /// Returns a mutable reference to the active external sort configuration.
    pub fn sort_config_mut(&mut self) -> &mut SortConfig {
        &mut self.sort_config
    }

    /// Sets the active external sort configuration.
    pub fn set_sort_config(&mut self, config: SortConfig) {
        self.sort_config = config;
    }

    /// Returns a reference to the active external aggregation configuration.
    pub fn aggregate_config(&self) -> &AggregateConfig {
        &self.aggregate_config
    }

    /// Returns a mutable reference to the active external aggregation configuration.
    pub fn aggregate_config_mut(&mut self) -> &mut AggregateConfig {
        &mut self.aggregate_config
    }

    /// Sets the active external aggregation configuration.
    pub fn set_aggregate_config(&mut self, config: AggregateConfig) {
        self.aggregate_config = config;
    }

    /// Returns a reference to the active external hash join configuration.
    pub fn join_config(&self) -> &JoinConfig {
        &self.join_config
    }

    /// Returns a mutable reference to the active external hash join configuration.
    pub fn join_config_mut(&mut self) -> &mut JoinConfig {
        &mut self.join_config
    }

    /// Sets the active external hash join configuration.
    pub fn set_join_config(&mut self, config: JoinConfig) {
        self.join_config = config;
    }

    /// Returns a reference to the active external multi-index candidate configuration.
    pub fn multi_index_config(&self) -> &MultiIndexConfig {
        &self.multi_index_config
    }

    /// Returns a mutable reference to the active external multi-index candidate configuration.
    pub fn multi_index_config_mut(&mut self) -> &mut MultiIndexConfig {
        &mut self.multi_index_config
    }

    /// Sets the active external multi-index candidate configuration.
    pub fn set_multi_index_config(&mut self, config: MultiIndexConfig) {
        self.multi_index_config = config;
    }

    /// Returns a reference to the active materialization configuration.
    pub fn materialize_config(&self) -> &crate::query::materialize::MaterializeConfig {
        &self.materialize_config
    }

    /// Returns a mutable reference to the active materialization configuration.
    pub fn materialize_config_mut(&mut self) -> &mut crate::query::materialize::MaterializeConfig {
        &mut self.materialize_config
    }

    /// Sets the active materialization configuration.
    pub fn set_materialize_config(&mut self, config: crate::query::materialize::MaterializeConfig) {
        self.materialize_config = config;
    }

    /// Sets a test-only materialization override.
    pub fn set_materialize_override(
        &mut self,
        test_override: crate::query::materialize::MaterializeOverride,
    ) {
        self.materialize_config.test_override = test_override;
    }

    /// Returns the deterministic resource planning signature for current memory configurations.
    pub fn resource_planning_signature(&self) -> crate::query::ResourcePlanningSignature {
        crate::query::ResourcePlanningSignature::from_configs(
            &self.sort_config,
            &self.aggregate_config,
            &self.join_config,
            &self.multi_index_config,
        )
    }
}
