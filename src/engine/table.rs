use crate::engine::catalog::{Catalog, IndexMetadata, TableId, TableMetadata};
use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::index::secondary::{IndexKey, SecondaryIndex};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::record_store::RecordStore;

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
                let val = &row.values[meta.column_idx];
                let idx_key = IndexKey::from_value(val);
                let existing = sec_idx.lookup(self.buffer_pool, &idx_key)?;
                if !existing.is_empty() {
                    return Err(Error::DuplicateIndexKey(format!("{val}")));
                }
            }
        }

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.insert(self.buffer_pool, key, rid)?;

        // Maintain secondary indexes
        for (meta, sec_idx) in &mut self.indexes {
            let val = &row.values[meta.column_idx];
            let idx_key = IndexKey::from_value(val);
            sec_idx.insert(self.buffer_pool, &idx_key, rid)?;
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
    /// 9. Maintains secondary indexes (deletes old `(old_key, old_rid)` and inserts `(new_key, new_rid)`).
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
            let old_val = &old_row.values[meta.column_idx];
            let new_val = &row.values[meta.column_idx];
            if meta.unique && old_val != new_val {
                let idx_key = IndexKey::from_value(new_val);
                let existing = sec_idx.lookup(self.buffer_pool, &idx_key)?;
                if existing.iter().any(|&r| r != old_rid) {
                    return Err(Error::DuplicateIndexKey(format!("{new_val}")));
                }
            }
        }

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let new_rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.update(self.buffer_pool, key, new_rid)?;
        self.record_store.delete(self.buffer_pool, old_rid)?;

        // Update secondary indexes
        for (meta, sec_idx) in &mut self.indexes {
            let old_val = &old_row.values[meta.column_idx];
            let new_val = &row.values[meta.column_idx];
            let old_key = IndexKey::from_value(old_val);
            let new_key = IndexKey::from_value(new_val);

            sec_idx.delete(self.buffer_pool, &old_key, old_rid)?;
            sec_idx.insert(self.buffer_pool, &new_key, new_rid)?;
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
                            let val = &decoded.values[meta.column_idx];
                            let idx_key = IndexKey::from_value(val);
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
}
