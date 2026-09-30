use std::collections::HashSet;

use crate::engine::catalog::{Catalog, IndexMetadata, TableId, TableMetadata};
use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::index::secondary::{IndexKey, SecondaryIndex};
use crate::query::stats::{ColumnStatistics, IndexStatistics, TableStatistics};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::record_store::RecordStore;
use crate::RecordId;

/// Runtime statistics collected during secondary index skip scan execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SkipScanStats {
    /// Number of distinct leading-prefix values discovered from the index.
    pub distinct_prefix_values: usize,
    /// Number of point lookups / prefix range scans executed across distinct prefix values.
    pub skip_scan_probes: usize,
    /// Total number of secondary index leaf entries examined across all probes.
    pub index_entries_examined: usize,
}

/// Runtime statistics collected during multi-index scan execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiIndexScanStats {
    /// Execution mode (InMemory vs External).
    pub execution_mode: crate::query::external_multi_index::MultiIndexExecutionMode,
    /// Number of participating secondary indexes.
    pub indexes_used: usize,
    /// Total candidate rows collected across all index scans before set operation.
    pub candidate_rows_before_set_operation: usize,
    /// Total candidate rows surviving the set operation (intersection or union).
    pub candidate_rows_after_set_operation: usize,
    /// Number of record IDs evaluated during intersection.
    pub record_ids_intersected: usize,
    /// Number of record IDs combined during union.
    pub record_ids_unioned: usize,
    /// Number of duplicate record IDs eliminated during union deduplication.
    pub duplicate_record_ids_removed: usize,
    /// Number of base table records fetched from RecordStore.
    pub base_records_fetched: usize,
    /// Number of temporary runs created during external spilling.
    pub spill_runs_created: usize,
    /// Total bytes written to spill files.
    pub spill_bytes_written: usize,
    /// Total bytes read from spill files.
    pub spill_bytes_read: usize,
    /// Total candidate RecordIds spilled.
    pub candidate_rows_spilled: usize,
    /// Total candidate RecordIds merged.
    pub candidate_rows_merged: usize,
    /// Peak in-memory byte footprint during candidate processing.
    pub peak_candidate_memory: usize,
}

impl Default for MultiIndexScanStats {
    fn default() -> Self {
        Self {
            execution_mode: crate::query::external_multi_index::MultiIndexExecutionMode::InMemory,
            indexes_used: 0,
            candidate_rows_before_set_operation: 0,
            candidate_rows_after_set_operation: 0,
            record_ids_intersected: 0,
            record_ids_unioned: 0,
            duplicate_record_ids_removed: 0,
            base_records_fetched: 0,
            spill_runs_created: 0,
            spill_bytes_written: 0,
            spill_bytes_read: 0,
            candidate_rows_spilled: 0,
            candidate_rows_merged: 0,
            peak_candidate_memory: 0,
        }
    }
}

/// High-level relational table handle coordinating schema validation,
/// canonical primary key indexing in the primary B+ Tree, tuple storage in the RecordStore,
/// and automatic secondary index maintenance.
#[derive(Debug)]
pub struct Table<'a> {
    metadata: TableMetadata,
    tree: BPlusTree,
    record_store: RecordStore,
    indexes: Vec<(IndexMetadata, SecondaryIndex)>,
    buffer_pool: &'a mut BufferPool,
}

impl<'a> Table<'a> {
    /// Creates a new `Table` handle wrapping table metadata, primary tree, record store,
    /// secondary indexes, and buffer pool.
    pub fn new(
        metadata: TableMetadata,
        tree: BPlusTree,
        record_store: RecordStore,
        indexes: Vec<(IndexMetadata, SecondaryIndex)>,
        buffer_pool: &'a mut BufferPool,
    ) -> Self {
        Self {
            metadata,
            tree,
            record_store,
            indexes,
            buffer_pool,
        }
    }

    /// Returns a reference to the table metadata.
    pub const fn metadata(&self) -> &TableMetadata {
        &self.metadata
    }

    /// Returns the logical identifier of this table.
    pub const fn table_id(&self) -> TableId {
        self.metadata.table_id
    }

    /// Returns the UTF-8 name of this table.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// Returns a reference to the table schema.
    pub const fn schema(&self) -> &Schema {
        &self.metadata.schema
    }

    /// Returns a slice of the secondary indexes attached to this table.
    pub fn indexes(&self) -> &[(IndexMetadata, SecondaryIndex)] {
        &self.indexes
    }

    /// Inserts a new row into the table and updates all attached secondary indexes.
    ///
    /// # Process
    /// 1. Validates the row against the table schema.
    /// 2. Extracts and encodes the canonical primary key.
    /// 3. Verifies that no existing row with the same primary key exists (`Error::DuplicatePrimaryKey`).
    /// 4. Validates unique constraints across all unique secondary indexes (`Error::DuplicateIndexKey`).
    /// 5. Serializes the row into a deterministic binary tuple format.
    /// 6. Inserts the tuple payload into the RecordStore.
    /// 7. Inserts the canonical primary key and `RecordId` into the primary B+ Tree.
    /// 8. Inserts `(IndexKey, RecordId)` entries into all secondary indexes.
    /// 9. Synchronizes table and index root metadata in the catalog if roots were split/modified.
    pub fn insert(&mut self, row: &Row) -> Result<()> {
        self.metadata.schema.validate_row(row)?;
        let key = self.metadata.schema.extract_primary_key(row)?;

        if self.tree.get(self.buffer_pool, key)?.is_some() {
            let pk_val = &row.values[self.metadata.schema.primary_key_idx];
            return Err(Error::DuplicatePrimaryKey(format!("{pk_val}")));
        }

        // Validate unique secondary index constraints
        for (meta, sec_idx) in &self.indexes {
            if meta.unique {
                let idx_key = meta.extract_index_key(row);
                let existing = sec_idx.lookup(self.buffer_pool, &idx_key)?;
                if !existing.is_empty() {
                    let vals_str = meta
                        .column_indices
                        .iter()
                        .map(|&i| format!("{}", row.values[i]))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(Error::DuplicateIndexKey(
                        if meta.column_indices.len() == 1 {
                            vals_str
                        } else {
                            format!("({vals_str})")
                        },
                    ));
                }
            }
        }

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.insert(self.buffer_pool, key, rid)?;

        // Maintain secondary indexes
        for (meta, sec_idx) in &mut self.indexes {
            let idx_key = meta.extract_index_key(row);
            let payload = meta.extract_covered_payload(row)?;
            sec_idx.insert_with_payload(self.buffer_pool, &idx_key, &payload, rid)?;
        }

        self.sync_metadata_if_needed()?;

        Ok(())
    }

    /// Retrieves an owned copy of the `Row` associated with the specified primary key `Value`.
    pub fn get(&mut self, pk: &Value) -> Result<Option<Row>> {
        self.validate_pk_type(pk)?;
        let key = pk.encode_to_canonical_key()?;

        match self.tree.get(self.buffer_pool, key)? {
            Some(rid) => match self.record_store.get(self.buffer_pool, rid)? {
                Some(bytes) => {
                    let row = self.metadata.schema.decode_tuple(&bytes)?;
                    Ok(Some(row))
                }
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Updates an existing row in the table matching the primary key in `row`.
    ///
    /// # Process
    /// 1. Validates the row against the table schema.
    /// 2. Extracts the canonical primary key.
    /// 3. Looks up the existing `RecordId` in the primary B+ Tree index.
    /// 4. Loads and decodes the old row to determine changed column values.
    /// 5. Validates unique constraints on changed secondary indexed columns.
    /// 6. Inserts the new tuple into the RecordStore.
    /// 7. Updates the primary B+ Tree index mapping to point to the new `RecordId`.
    /// 8. Deletes the old record from the RecordStore.
    /// 9. Maintains secondary indexes (deletes old `(old_key, old_rid)` and inserts `(new_key, new_rid)` with updated payload).
    /// 10. Synchronizes catalog metadata.
    pub fn update(&mut self, row: &Row) -> Result<()> {
        self.metadata.schema.validate_row(row)?;
        let key = self.metadata.schema.extract_primary_key(row)?;

        let old_rid = self
            .tree
            .get(self.buffer_pool, key)?
            .ok_or(Error::RowNotFound)?;

        let old_bytes = self
            .record_store
            .get(self.buffer_pool, old_rid)?
            .ok_or(Error::RowNotFound)?;
        let old_row = self.metadata.schema.decode_tuple(&old_bytes)?;

        // Validate uniqueness on changed indexed values
        for (meta, sec_idx) in &self.indexes {
            let old_key = meta.extract_index_key(&old_row);
            let new_key = meta.extract_index_key(row);
            if meta.unique && old_key != new_key {
                let existing = sec_idx.lookup(self.buffer_pool, &new_key)?;
                if existing.iter().any(|&r| r != old_rid) {
                    let vals_str = meta
                        .column_indices
                        .iter()
                        .map(|&i| format!("{}", row.values[i]))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(Error::DuplicateIndexKey(
                        if meta.column_indices.len() == 1 {
                            vals_str
                        } else {
                            format!("({vals_str})")
                        },
                    ));
                }
            }
        }

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let new_rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.update(self.buffer_pool, key, new_rid)?;
        self.record_store.delete(self.buffer_pool, old_rid)?;

        // Update secondary indexes
        for (meta, sec_idx) in &mut self.indexes {
            let old_key = meta.extract_index_key(&old_row);
            let new_key = meta.extract_index_key(row);
            let new_payload = meta.extract_covered_payload(row)?;

            sec_idx.delete(self.buffer_pool, &old_key, old_rid)?;
            sec_idx.insert_with_payload(self.buffer_pool, &new_key, &new_payload, new_rid)?;
        }

        self.sync_metadata_if_needed()?;

        Ok(())
    }

    /// Deletes a row matching the specified primary key `Value` from the table and secondary indexes.
    pub fn delete(&mut self, pk: &Value) -> Result<Option<Row>> {
        self.validate_pk_type(pk)?;
        let key = pk.encode_to_canonical_key()?;

        match self.tree.delete(self.buffer_pool, key)? {
            Some(rid) => {
                let bytes = self.record_store.get(self.buffer_pool, rid)?;
                self.record_store.delete(self.buffer_pool, rid)?;

                let row = match bytes {
                    Some(b) => {
                        let decoded = self.metadata.schema.decode_tuple(&b)?;
                        // Remove from all secondary indexes
                        for (meta, sec_idx) in &mut self.indexes {
                            let idx_key = meta.extract_index_key(&decoded);
                            sec_idx.delete(self.buffer_pool, &idx_key, rid)?;
                        }
                        Some(decoded)
                    }
                    None => None,
                };

                self.sync_metadata_if_needed()?;
                Ok(row)
            }
            None => Ok(None),
        }
    }

    /// Performs an ordered range scan over primary keys in `[start, end)`.
    pub fn scan(&mut self, start: Option<&Value>, end: Option<&Value>) -> Result<Vec<Row>> {
        let start_key = match start {
            Some(v) => {
                self.validate_pk_type(v)?;
                Some(v.encode_to_canonical_key()?)
            }
            None => None,
        };

        let end_key = match end {
            Some(v) => {
                self.validate_pk_type(v)?;
                Some(v.encode_to_canonical_key()?)
            }
            None => None,
        };

        let rids = self.tree.scan(self.buffer_pool, start_key, end_key)?;
        let mut rows = Vec::with_capacity(rids.len());

        for (_k, rid) in rids {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// Performs a point lookup on a secondary index, returning all matching rows.
    pub fn index_lookup(&mut self, index_name: &str, value: &Value) -> Result<Vec<Row>> {
        let sec_idx = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let idx_key = IndexKey::from_value(value);
        let rids = sec_idx.lookup(self.buffer_pool, &idx_key)?;
        let mut rows = Vec::with_capacity(rids.len());

        for rid in rids {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// Performs a point lookup on a secondary index, returning matching record IDs without decoding tuples.
    pub fn index_lookup_rids(&mut self, index_name: &str, value: &Value) -> Result<Vec<RecordId>> {
        let sec_idx = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let idx_key = IndexKey::from_value(value);
        sec_idx.lookup(self.buffer_pool, &idx_key)
    }

    /// Fetches and decodes a row tuple given its record storage ID.
    pub fn get_row_by_rid(&mut self, rid: RecordId) -> Result<Option<Row>> {
        if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
            let row = self.metadata.schema.decode_tuple(&bytes)?;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    /// Performs an ordered range scan on a secondary index in `[start, end)`.
    pub fn index_scan(
        &mut self,
        index_name: &str,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> Result<Vec<Row>> {
        let sec_idx = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let start_key = start.map(IndexKey::from_value);
        let end_key = end.map(IndexKey::from_value);

        let entries = sec_idx.scan(self.buffer_pool, start_key.as_ref(), end_key.as_ref())?;
        let mut rows = Vec::with_capacity(entries.len());

        for (_k, rid) in entries {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// Performs a composite point or prefix lookup on a secondary index, returning matching record IDs.
    pub fn composite_index_lookup_rids(
        &mut self,
        index_name: &str,
        prefix_values: &[Value],
    ) -> Result<Vec<RecordId>> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        if prefix_values.is_empty() {
            return Err(Error::InvalidIndexDefinition(
                "prefix values cannot be empty".to_string(),
            ));
        }

        if prefix_values.len() == meta.column_indices.len() {
            let key = crate::index::composite::CompositeIndexKey::from_values(prefix_values)
                .to_index_key(meta.column_indices.len());
            sec_idx.lookup(self.buffer_pool, &key)
        } else {
            let start_bytes = crate::index::composite::CompositeIndexKey::encode_prefix_values(
                prefix_values,
                meta.column_indices.len(),
            );
            let end_bytes =
                crate::index::composite::CompositeIndexKey::prefix_successor(&start_bytes);
            let start_key = IndexKey::new(start_bytes);
            let end_key = end_bytes.map(IndexKey::new);
            let entries = sec_idx.scan(self.buffer_pool, Some(&start_key), end_key.as_ref())?;
            Ok(entries.into_iter().map(|(_k, rid)| rid).collect())
        }
    }

    /// Performs a composite point or prefix lookup on a secondary index, returning all matching rows.
    pub fn composite_index_lookup(
        &mut self,
        index_name: &str,
        prefix_values: &[Value],
    ) -> Result<Vec<Row>> {
        let rids = self.composite_index_lookup_rids(index_name, prefix_values)?;
        let mut rows = Vec::with_capacity(rids.len());
        for rid in rids {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Performs a bounded range scan on a composite secondary index.
    pub fn composite_index_scan(
        &mut self,
        index_name: &str,
        start_key: Option<&IndexKey>,
        end_key: Option<&IndexKey>,
    ) -> Result<Vec<Row>> {
        let sec_idx = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let entries = sec_idx.scan(self.buffer_pool, start_key, end_key)?;
        let mut rows = Vec::with_capacity(entries.len());

        for (_k, rid) in entries {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// Performs a bounded range scan on a composite secondary index, returning record IDs directly.
    pub fn composite_index_scan_rids(
        &mut self,
        index_name: &str,
        start_key: Option<&IndexKey>,
        end_key: Option<&IndexKey>,
    ) -> Result<Vec<RecordId>> {
        let sec_idx = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let entries = sec_idx.scan(self.buffer_pool, start_key, end_key)?;
        Ok(entries.into_iter().map(|(_k, rid)| rid).collect())
    }

    /// Performs an index-only scan on a secondary index, reconstructing rows directly
    /// from decoded index keys and included payloads without fetching from `RecordStore`.
    pub fn index_only_scan(
        &mut self,
        index_name: &str,
        scan_type: &crate::query::optimizer::SecondaryScanType,
    ) -> Result<Vec<Row>> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        if let crate::query::optimizer::SecondaryScanType::SkipScan {
            skipped_prefix_columns,
            constrained_columns,
        } = scan_type
        {
            let constrained_values: Vec<Value> =
                constrained_columns.iter().map(|(_, v)| v.clone()).collect();
            let (rows, _) = self.index_only_skip_scan(
                index_name,
                skipped_prefix_columns.len(),
                &constrained_values,
            )?;
            return Ok(rows);
        }

        let entries: Vec<crate::index::secondary::SecondaryLeafEntry> = match scan_type {
            crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                let idx_key = if meta.column_indices.len() == 1 {
                    IndexKey::from_value(val)
                } else {
                    crate::index::composite::CompositeIndexKey::from_values(std::slice::from_ref(
                        val,
                    ))
                    .to_index_key(meta.column_indices.len())
                };
                sec_idx.lookup_entries(self.buffer_pool, &idx_key)?
            }
            crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals) => {
                let key = crate::index::composite::CompositeIndexKey::from_values(vals)
                    .to_index_key(meta.column_indices.len());
                sec_idx.lookup_entries(self.buffer_pool, &key)?
            }
            crate::query::optimizer::SecondaryScanType::PrefixScan(prefix_vals) => {
                if prefix_vals.len() == meta.column_indices.len() {
                    let key = crate::index::composite::CompositeIndexKey::from_values(prefix_vals)
                        .to_index_key(meta.column_indices.len());
                    sec_idx.lookup_entries(self.buffer_pool, &key)?
                } else {
                    let start_bytes =
                        crate::index::composite::CompositeIndexKey::encode_prefix_values(
                            prefix_vals,
                            meta.column_indices.len(),
                        );
                    let end_bytes =
                        crate::index::composite::CompositeIndexKey::prefix_successor(&start_bytes);
                    let start_key = IndexKey::new(start_bytes);
                    let end_key = end_bytes.map(IndexKey::new);
                    sec_idx.scan_entries(self.buffer_pool, Some(&start_key), end_key.as_ref())?
                }
            }
            crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                let start_key = start.as_ref().map(IndexKey::from_value);
                let end_key = end.as_ref().map(IndexKey::from_value);
                sec_idx.scan_entries(self.buffer_pool, start_key.as_ref(), end_key.as_ref())?
            }
            crate::query::optimizer::SecondaryScanType::SkipScan { .. } => {
                unreachable!("SkipScan is handled above")
            }
        };

        let schema_len = self.metadata.schema.columns.len();
        let key_col_types: Vec<crate::engine::types::DataType> = meta
            .column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();
        let inc_col_types: Vec<crate::engine::types::DataType> = meta
            .include_column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();

        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut row_values = vec![Value::Null; schema_len];

            // Decode key columns
            let key_values = if meta.column_indices.len() == 1 {
                let val = entry.key.to_value(key_col_types[0])?;
                vec![val]
            } else {
                crate::index::composite::CompositeIndexKey::decode_values(
                    &entry.key.bytes,
                    &key_col_types,
                )?
            };

            for (&col_idx, val) in meta.column_indices.iter().zip(key_values) {
                row_values[col_idx] = val;
            }

            // Decode included columns
            if !meta.include_column_indices.is_empty() && !entry.payload.is_empty() {
                let inc_values =
                    crate::index::covering::CoveredPayload::decode(&entry.payload, &inc_col_types)?;
                for (&inc_idx, val) in meta.include_column_indices.iter().zip(inc_values) {
                    row_values[inc_idx] = val;
                }
            }

            rows.push(Row::new(row_values));
        }

        Ok(rows)
    }

    /// Performs an index skip scan over a composite secondary index, enumerating distinct leading-prefix
    /// values and probing for `(prefix, constrained_suffix)`. Fetches base records from `RecordStore`.
    pub fn index_skip_scan(
        &mut self,
        index_name: &str,
        skipped_prefix_len: usize,
        constrained_suffix: &[Value],
    ) -> Result<(Vec<Row>, SkipScanStats)> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let key_col_types: Vec<crate::engine::types::DataType> = meta
            .column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();

        let distinct_prefixes = sec_idx.distinct_prefix_values(
            self.buffer_pool,
            meta.column_indices.len(),
            skipped_prefix_len,
            &key_col_types,
        )?;

        let mut stats = SkipScanStats {
            distinct_prefix_values: distinct_prefixes.len(),
            skip_scan_probes: 0,
            index_entries_examined: 0,
        };

        let mut rows = Vec::new();
        let total_cols = meta.column_indices.len();

        for prefix in distinct_prefixes {
            stats.skip_scan_probes += 1;
            let mut probe_values = prefix;
            probe_values.extend_from_slice(constrained_suffix);

            let entries = if probe_values.len() == total_cols {
                let key = crate::index::composite::CompositeIndexKey::from_values(&probe_values)
                    .to_index_key(total_cols);
                sec_idx.lookup_entries(self.buffer_pool, &key)?
            } else {
                let start_bytes = crate::index::composite::CompositeIndexKey::encode_prefix_values(
                    &probe_values,
                    total_cols,
                );
                let end_bytes =
                    crate::index::composite::CompositeIndexKey::prefix_successor(&start_bytes);
                let start_key = IndexKey::new(start_bytes);
                let end_key = end_bytes.map(IndexKey::new);
                sec_idx.scan_entries(self.buffer_pool, Some(&start_key), end_key.as_ref())?
            };

            stats.index_entries_examined += entries.len();

            for entry in entries {
                if let Some(bytes) = self.record_store.get(self.buffer_pool, entry.rid)? {
                    let row = self.metadata.schema.decode_tuple(&bytes)?;
                    rows.push(row);
                }
            }
        }

        Ok((rows, stats))
    }

    /// Performs an index-only skip scan over a composite secondary index, reconstructing rows directly
    /// from decoded index keys and included payloads without fetching base records from `RecordStore`.
    pub fn index_only_skip_scan(
        &mut self,
        index_name: &str,
        skipped_prefix_len: usize,
        constrained_suffix: &[Value],
    ) -> Result<(Vec<Row>, SkipScanStats)> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let schema_len = self.metadata.schema.columns.len();
        let key_col_types: Vec<crate::engine::types::DataType> = meta
            .column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();
        let inc_col_types: Vec<crate::engine::types::DataType> = meta
            .include_column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();

        let distinct_prefixes = sec_idx.distinct_prefix_values(
            self.buffer_pool,
            meta.column_indices.len(),
            skipped_prefix_len,
            &key_col_types,
        )?;

        let mut stats = SkipScanStats {
            distinct_prefix_values: distinct_prefixes.len(),
            skip_scan_probes: 0,
            index_entries_examined: 0,
        };

        let mut rows = Vec::new();
        let total_cols = meta.column_indices.len();

        for prefix in distinct_prefixes {
            stats.skip_scan_probes += 1;
            let mut probe_values = prefix;
            probe_values.extend_from_slice(constrained_suffix);

            let entries = if probe_values.len() == total_cols {
                let key = crate::index::composite::CompositeIndexKey::from_values(&probe_values)
                    .to_index_key(total_cols);
                sec_idx.lookup_entries(self.buffer_pool, &key)?
            } else {
                let start_bytes = crate::index::composite::CompositeIndexKey::encode_prefix_values(
                    &probe_values,
                    total_cols,
                );
                let end_bytes =
                    crate::index::composite::CompositeIndexKey::prefix_successor(&start_bytes);
                let start_key = IndexKey::new(start_bytes);
                let end_key = end_bytes.map(IndexKey::new);
                sec_idx.scan_entries(self.buffer_pool, Some(&start_key), end_key.as_ref())?
            };

            stats.index_entries_examined += entries.len();

            for entry in entries {
                let mut row_values = vec![Value::Null; schema_len];

                // Decode key columns
                let key_values = crate::index::composite::CompositeIndexKey::decode_values(
                    &entry.key.bytes,
                    &key_col_types,
                )?;

                for (&col_idx, val) in meta.column_indices.iter().zip(key_values) {
                    row_values[col_idx] = val;
                }

                // Decode included columns
                if !meta.include_column_indices.is_empty() && !entry.payload.is_empty() {
                    let inc_values = crate::index::covering::CoveredPayload::decode(
                        &entry.payload,
                        &inc_col_types,
                    )?;
                    for (&inc_idx, val) in meta.include_column_indices.iter().zip(inc_values) {
                        row_values[inc_idx] = val;
                    }
                }

                rows.push(Row::new(row_values));
            }
        }

        Ok((rows, stats))
    }

    /// Performs an index skip scan over a composite secondary index, returning candidate `RecordId`s directly
    /// and the total count of index entries examined.
    pub fn index_skip_scan_rids(
        &mut self,
        index_name: &str,
        skipped_prefix_len: usize,
        constrained_suffix: &[Value],
    ) -> Result<(Vec<RecordId>, usize)> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        let key_col_types: Vec<crate::engine::types::DataType> = meta
            .column_indices
            .iter()
            .map(|&i| self.metadata.schema.columns[i].data_type)
            .collect();

        let distinct_prefixes = sec_idx.distinct_prefix_values(
            self.buffer_pool,
            meta.column_indices.len(),
            skipped_prefix_len,
            &key_col_types,
        )?;

        let mut rids = Vec::new();
        let total_cols = meta.column_indices.len();
        let mut entries_examined = 0;

        for prefix in distinct_prefixes {
            let mut probe_values = prefix;
            probe_values.extend_from_slice(constrained_suffix);

            let entries = if probe_values.len() == total_cols {
                let key = crate::index::composite::CompositeIndexKey::from_values(&probe_values)
                    .to_index_key(total_cols);
                sec_idx.lookup_entries(self.buffer_pool, &key)?
            } else {
                let start_bytes = crate::index::composite::CompositeIndexKey::encode_prefix_values(
                    &probe_values,
                    total_cols,
                );
                let end_bytes =
                    crate::index::composite::CompositeIndexKey::prefix_successor(&start_bytes);
                let start_key = IndexKey::new(start_bytes);
                let end_key = end_bytes.map(IndexKey::new);
                sec_idx.scan_entries(self.buffer_pool, Some(&start_key), end_key.as_ref())?
            };

            entries_examined += entries.len();
            for entry in entries {
                rids.push(entry.rid);
            }
        }

        Ok((rids, entries_examined))
    }

    /// Scans a secondary index using the specified scan mode, returning candidate `RecordId`s directly
    /// without fetching base tuples from `RecordStore`.
    pub fn scan_index_record_ids(
        &mut self,
        index_name: &str,
        scan_type: &crate::query::optimizer::SecondaryScanType,
    ) -> Result<(Vec<RecordId>, usize)> {
        let (meta, sec_idx) = self
            .indexes
            .iter()
            .find(|(m, _)| m.name == index_name)
            .map(|(m, s)| (m.clone(), *s))
            .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;

        match scan_type {
            crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                if meta.column_indices.len() == 1 {
                    let key = IndexKey::from_value(val);
                    let rids = sec_idx.lookup(self.buffer_pool, &key)?;
                    let count = rids.len();
                    Ok((rids, count))
                } else {
                    let rids =
                        self.composite_index_lookup_rids(index_name, std::slice::from_ref(val))?;
                    let count = rids.len();
                    Ok((rids, count))
                }
            }
            crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
            | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                let rids = self.composite_index_lookup_rids(index_name, vals)?;
                let count = rids.len();
                Ok((rids, count))
            }
            crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                let start_key = start.as_ref().map(IndexKey::from_value);
                let end_key = end.as_ref().map(IndexKey::from_value);
                let entries =
                    sec_idx.scan(self.buffer_pool, start_key.as_ref(), end_key.as_ref())?;
                let count = entries.len();
                let rids = entries.into_iter().map(|(_k, rid)| rid).collect();
                Ok((rids, count))
            }
            crate::query::optimizer::SecondaryScanType::SkipScan {
                skipped_prefix_columns,
                constrained_columns,
            } => {
                let constrained_values: Vec<Value> =
                    constrained_columns.iter().map(|(_, v)| v.clone()).collect();
                self.index_skip_scan_rids(
                    index_name,
                    skipped_prefix_columns.len(),
                    &constrained_values,
                )
            }
        }
    }

    /// Executes a multi-index scan combining multiple secondary indexes via Intersection or Union with default config.
    pub fn multi_index_scan(
        &mut self,
        strategy: crate::query::optimizer::MultiIndexStrategy,
        child_scans: &[crate::query::optimizer::MultiIndexChildScan],
    ) -> Result<(Vec<Row>, MultiIndexScanStats)> {
        self.multi_index_scan_with_config(
            strategy,
            child_scans,
            &crate::query::external_multi_index::MultiIndexConfig::default(),
        )
    }

    /// Executes a multi-index scan combining multiple secondary indexes via Intersection or Union
    /// using the specified `MultiIndexConfig` (supporting both in-memory fast-path and external disk spilling).
    pub fn multi_index_scan_with_config(
        &mut self,
        strategy: crate::query::optimizer::MultiIndexStrategy,
        child_scans: &[crate::query::optimizer::MultiIndexChildScan],
        config: &crate::query::external_multi_index::MultiIndexConfig,
    ) -> Result<(Vec<Row>, MultiIndexScanStats)> {
        let mut stats = MultiIndexScanStats {
            indexes_used: child_scans.len(),
            ..Default::default()
        };

        if child_scans.is_empty() {
            return Ok((Vec::new(), stats));
        }

        let mut child_rid_lists: Vec<Vec<RecordId>> = Vec::with_capacity(child_scans.len());
        for child in child_scans {
            let (rids, _examined) =
                self.scan_index_record_ids(&child.index_name, &child.scan_type)?;
            stats.candidate_rows_before_set_operation += rids.len();
            child_rid_lists.push(rids);
        }

        let total_candidate_bytes = stats.candidate_rows_before_set_operation
            * crate::query::external_multi_index::RECORD_ID_IN_MEMORY_BYTES;
        stats.peak_candidate_memory = total_candidate_bytes;

        let final_rids: Vec<RecordId> = if total_candidate_bytes <= config.memory_budget_bytes {
            // In-Memory fast path
            stats.execution_mode =
                crate::query::external_multi_index::MultiIndexExecutionMode::InMemory;
            match strategy {
                crate::query::optimizer::MultiIndexStrategy::Intersection => {
                    if child_rid_lists.is_empty() {
                        Vec::new()
                    } else {
                        let mut current_set: std::collections::BTreeSet<RecordId> =
                            child_rid_lists[0].iter().copied().collect();
                        for next_list in &child_rid_lists[1..] {
                            let next_set: std::collections::BTreeSet<RecordId> =
                                next_list.iter().copied().collect();
                            current_set = current_set.intersection(&next_set).copied().collect();
                        }
                        stats.record_ids_intersected = stats.candidate_rows_before_set_operation;
                        stats.candidate_rows_after_set_operation = current_set.len();
                        current_set.into_iter().collect()
                    }
                }
                crate::query::optimizer::MultiIndexStrategy::Union => {
                    let mut seen = std::collections::BTreeSet::new();
                    let mut union_rids = Vec::new();
                    for rid in child_rid_lists.into_iter().flatten() {
                        if seen.insert(rid) {
                            union_rids.push(rid);
                        } else {
                            stats.duplicate_record_ids_removed += 1;
                        }
                    }
                    stats.record_ids_unioned = stats.candidate_rows_before_set_operation;
                    stats.candidate_rows_after_set_operation = union_rids.len();
                    union_rids
                }
            }
        } else {
            // External spill path
            stats.execution_mode =
                crate::query::external_multi_index::MultiIndexExecutionMode::External;
            let temp_dir = config.effective_temp_dir();
            let per_child_budget = (config.memory_budget_bytes / child_scans.len().max(1)).max(1);
            let mut spill_metrics =
                crate::query::external_multi_index::MultiIndexSpillMetrics::default();

            let mut streams: Vec<crate::query::external_multi_index::RecordIdStream> =
                Vec::with_capacity(child_rid_lists.len());
            // Keep RAII run files alive until the end of function
            let mut all_run_files: Vec<crate::query::external_multi_index::TempMultiIndexRunFile> =
                Vec::new();

            for rids in child_rid_lists {
                let (in_mem, run_files) =
                    crate::query::external_multi_index::prepare_child_candidate_stream(
                        rids,
                        per_child_budget,
                        &temp_dir,
                        &mut spill_metrics,
                    )?;
                if let Some(mem_rids) = in_mem {
                    streams.push(
                        crate::query::external_multi_index::RecordIdStream::from_vec(mem_rids),
                    );
                } else if !run_files.is_empty() {
                    if run_files.len() == 1 {
                        let reader = crate::query::external_multi_index::RecordIdRunReader::open(
                            run_files[0].path(),
                        )?;
                        streams.push(
                            crate::query::external_multi_index::RecordIdStream::SpillReader(reader),
                        );
                    } else {
                        let mut readers = Vec::with_capacity(run_files.len());
                        for rf in &run_files {
                            readers.push(
                                crate::query::external_multi_index::RecordIdRunReader::open(
                                    rf.path(),
                                )?,
                            );
                        }
                        let merger =
                            crate::query::external_multi_index::KWayRecordIdMerger::new(readers)?;
                        streams.push(
                            crate::query::external_multi_index::RecordIdStream::KWayMerger(merger),
                        );
                    }
                    all_run_files.extend(run_files);
                }
            }

            match strategy {
                crate::query::optimizer::MultiIndexStrategy::Intersection => {
                    let (res, duplicates) =
                        crate::query::external_multi_index::execute_multi_index_intersection_streams(
                            streams,
                        )?;
                    stats.record_ids_intersected = stats.candidate_rows_before_set_operation;
                    stats.candidate_rows_after_set_operation = res.len();
                    stats.duplicate_record_ids_removed = duplicates;
                    stats.spill_runs_created = spill_metrics.runs_created;
                    stats.spill_bytes_written = spill_metrics.bytes_written;
                    stats.spill_bytes_read = spill_metrics.bytes_read;
                    stats.candidate_rows_spilled = spill_metrics.rows_spilled;
                    stats.candidate_rows_merged = spill_metrics.rows_merged;
                    stats.peak_candidate_memory = stats
                        .peak_candidate_memory
                        .max(spill_metrics.peak_memory_bytes);
                    res
                }
                crate::query::optimizer::MultiIndexStrategy::Union => {
                    let (res, duplicates) =
                        crate::query::external_multi_index::execute_multi_index_union_streams(
                            streams,
                        )?;
                    stats.record_ids_unioned = stats.candidate_rows_before_set_operation;
                    stats.candidate_rows_after_set_operation = res.len();
                    stats.duplicate_record_ids_removed = duplicates;
                    stats.spill_runs_created = spill_metrics.runs_created;
                    stats.spill_bytes_written = spill_metrics.bytes_written;
                    stats.spill_bytes_read = spill_metrics.bytes_read;
                    stats.candidate_rows_spilled = spill_metrics.rows_spilled;
                    stats.candidate_rows_merged = spill_metrics.rows_merged;
                    stats.peak_candidate_memory = stats
                        .peak_candidate_memory
                        .max(spill_metrics.peak_memory_bytes);
                    res
                }
            }
        };

        // Fetch base records from RecordStore only for surviving candidate RecordIds
        let mut rows = Vec::with_capacity(final_rids.len());
        for rid in final_rids {
            if let Some(bytes) = self.record_store.get(self.buffer_pool, rid)? {
                let row = self.metadata.schema.decode_tuple(&bytes)?;
                rows.push(row);
                stats.base_records_fetched += 1;
            }
        }

        Ok((rows, stats))
    }

    /// Validates the structural invariants of the table's underlying primary B+ Tree.
    pub fn validate_tree(&mut self) -> Result<()> {
        self.tree.validate_tree(self.buffer_pool)
    }

    /// Validates the structural invariants of all attached secondary indexes.
    pub fn validate_indexes(&mut self) -> Result<()> {
        for (meta, sec_idx) in &self.indexes {
            sec_idx.validate_tree(self.buffer_pool, meta.unique)?;
        }
        Ok(())
    }

    fn validate_pk_type(&self, pk: &Value) -> Result<()> {
        let expected = self.metadata.schema.primary_key_column().data_type;
        let found = pk.data_type();

        if pk.is_null() {
            return Err(Error::NullConstraintViolation {
                column: self.metadata.schema.primary_key_column().name.clone(),
            });
        }

        if found != expected {
            return Err(Error::TypeMismatch {
                column: self.metadata.schema.primary_key_column().name.clone(),
                expected: expected.to_string(),
                found: found.to_string(),
            });
        }

        Ok(())
    }

    fn sync_metadata_if_needed(&mut self) -> Result<()> {
        let tree_root = self.tree.root_page_id();
        let rec_page = self.record_store.last_page_id();
        let mut changed = false;

        if tree_root != self.metadata.root_page_id {
            self.metadata.root_page_id = tree_root;
            changed = true;
        }

        if rec_page != 0 && rec_page != self.metadata.record_page_id {
            self.metadata.record_page_id = rec_page;
            changed = true;
        }

        if changed {
            Catalog::update_table_metadata(self.buffer_pool, &self.metadata)?;
        }

        for (meta, sec_idx) in &mut self.indexes {
            let cur_root = sec_idx.root_page_id();
            if cur_root != meta.root_page_id {
                meta.root_page_id = cur_root;
                Catalog::update_index_metadata(self.buffer_pool, meta)?;
            }
        }

        Ok(())
    }

    /// Inspects the table and derives comprehensive relational statistics without mutating persistent storage.
    pub fn collect_statistics(&mut self) -> Result<TableStatistics> {
        let schema = self.schema().clone();
        let num_cols = schema.columns.len();
        let mut null_counts = vec![0usize; num_cols];
        let mut distinct_sets: Vec<HashSet<crate::query::CanonicalValue>> =
            vec![HashSet::new(); num_cols];
        let mut mins: Vec<Option<Value>> = vec![None; num_cols];
        let mut maxs: Vec<Option<Value>> = vec![None; num_cols];

        let rows = self.scan(None, None)?;
        let row_count = rows.len();

        for row in &rows {
            for (idx, val) in row.values.iter().enumerate() {
                if idx < num_cols {
                    if val.is_null() {
                        null_counts[idx] += 1;
                    } else {
                        distinct_sets[idx].insert(crate::query::CanonicalValue::from_value(val));
                        match &mins[idx] {
                            Some(cur_min) => {
                                if let Ok(std::cmp::Ordering::Less) =
                                    crate::query::compare_values_for_sort(
                                        val,
                                        cur_min,
                                        crate::query::SortDirection::Ascending,
                                        None,
                                    )
                                {
                                    mins[idx] = Some(val.clone());
                                }
                            }
                            None => mins[idx] = Some(val.clone()),
                        }
                        match &maxs[idx] {
                            Some(cur_max) => {
                                if let Ok(std::cmp::Ordering::Greater) =
                                    crate::query::compare_values_for_sort(
                                        val,
                                        cur_max,
                                        crate::query::SortDirection::Ascending,
                                        None,
                                    )
                                {
                                    maxs[idx] = Some(val.clone());
                                }
                            }
                            None => maxs[idx] = Some(val.clone()),
                        }
                    }
                }
            }
        }

        let mut table_stats = TableStatistics::new(self.name(), row_count);

        for (idx, col) in schema.columns.iter().enumerate() {
            let col_stats = ColumnStatistics::new(
                row_count,
                null_counts[idx],
                distinct_sets[idx].len(),
                mins[idx].clone(),
                maxs[idx].clone(),
            );
            table_stats = table_stats.with_column(&col.name, col_stats);
        }

        for (meta, _sec_idx) in &self.indexes {
            let col_count = meta.column_indices.len();
            if col_count == 1 {
                let col_idx = meta.column_indices[0];
                let col_nulls = null_counts[col_idx];
                let col_distinct = distinct_sets[col_idx].len();
                let idx_stats = IndexStatistics::new(
                    row_count.saturating_sub(col_nulls),
                    col_distinct,
                    col_nulls,
                );
                table_stats = table_stats.with_index(&meta.name, idx_stats);
            } else {
                let mut composite_keys = HashSet::new();
                let mut null_rows = 0;
                for row in &rows {
                    let has_null = meta.column_indices.iter().any(|&i| row.values[i].is_null());
                    if has_null {
                        null_rows += 1;
                    }
                    let key = meta.extract_index_key(row);
                    composite_keys.insert(key);
                }
                let distinct_composite = composite_keys.len();
                let first_col_idx = meta.column_indices[0];
                let scalar_ndv = distinct_sets[first_col_idx].len();
                let est_leaves = ((row_count + 100) / 100).max(1);
                let idx_stats = IndexStatistics::new_composite(
                    row_count.saturating_sub(null_rows),
                    scalar_ndv,
                    null_rows,
                    col_count,
                    distinct_composite,
                    2,
                    est_leaves,
                );
                table_stats = table_stats.with_index(&meta.name, idx_stats);
            }
        }

        Ok(table_stats)
    }
}
