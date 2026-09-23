use crate::engine::catalog::{TableId, TableMetadata};
use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::storage::buffer_pool::BufferPool;
use crate::storage::record_store::RecordStore;

/// High-level relational table handle coordinating schema validation,
/// canonical primary key indexing in the B+ Tree, and tuple storage in the RecordStore.
#[derive(Debug)]
pub struct Table<'a> {
    metadata: TableMetadata,
    tree: BPlusTree,
    record_store: RecordStore,
    buffer_pool: &'a mut BufferPool,
}

impl<'a> Table<'a> {
    /// Creates a new `Table` handle wrapping table metadata, index, record store, and buffer pool.
    pub fn new(
        metadata: TableMetadata,
        tree: BPlusTree,
        record_store: RecordStore,
        buffer_pool: &'a mut BufferPool,
    ) -> Self {
        Self {
            metadata,
            tree,
            record_store,
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

    /// Inserts a new row into the table.
    ///
    /// # Process
    /// 1. Validates the row against the table schema.
    /// 2. Extracts and encodes the canonical primary key.
    /// 3. Verifies that no existing row with the same primary key exists (`Error::DuplicatePrimaryKey`).
    /// 4. Serializes the row into a deterministic binary tuple format.
    /// 5. Inserts the tuple payload into the RecordStore (handling inline or overflow storage transparently).
    /// 6. Inserts the canonical key and `RecordId` pointer into the table's B+ Tree index.
    ///
    /// # Errors
    /// * `Error::DuplicatePrimaryKey` if a row with the given primary key already exists.
    /// * `Error::ColumnCountMismatch`, `Error::TypeMismatch`, `Error::NullConstraintViolation` if the row violates schema constraints.
    pub fn insert(&mut self, row: &Row) -> Result<()> {
        self.metadata.schema.validate_row(row)?;
        let key = self.metadata.schema.extract_primary_key(row)?;

        if self.tree.get(self.buffer_pool, key)?.is_some() {
            let pk_val = &row.values[self.metadata.schema.primary_key_idx];
            return Err(Error::DuplicatePrimaryKey(format!("{pk_val}")));
        }

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.insert(self.buffer_pool, key, rid)?;
        self.sync_metadata_if_needed()?;

        Ok(())
    }

    /// Retrieves an owned copy of the `Row` associated with the specified primary key `Value`.
    ///
    /// # Errors
    /// * `Error::TypeMismatch` or `Error::InvalidPrimaryKey` if the provided `Value` does not match the PK column data type.
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
    /// 3. Looks up the existing `RecordId` in the B+ Tree index (`Error::RowNotFound` if missing).
    /// 4. Encodes the new row into the binary tuple format.
    /// 5. Inserts the new tuple into the RecordStore.
    /// 6. Updates the B+ Tree index mapping to point to the new `RecordId`.
    /// 7. Deletes the old record (and any overflow pages) from the RecordStore.
    ///
    /// # Errors
    /// * `Error::RowNotFound` if no row exists with the specified primary key.
    pub fn update(&mut self, row: &Row) -> Result<()> {
        self.metadata.schema.validate_row(row)?;
        let key = self.metadata.schema.extract_primary_key(row)?;

        let old_rid = self
            .tree
            .get(self.buffer_pool, key)?
            .ok_or(Error::RowNotFound)?;

        let tuple_bytes = self.metadata.schema.encode_tuple(row)?;
        let new_rid = self.record_store.insert(self.buffer_pool, &tuple_bytes)?;
        self.tree.update(self.buffer_pool, key, new_rid)?;
        self.record_store.delete(self.buffer_pool, old_rid)?;
        self.sync_metadata_if_needed()?;

        Ok(())
    }

    /// Deletes a row matching the specified primary key `Value` from the table.
    ///
    /// Returns `Ok(Some(Row))` containing the deleted row if present, or `Ok(None)` if absent.
    pub fn delete(&mut self, pk: &Value) -> Result<Option<Row>> {
        self.validate_pk_type(pk)?;
        let key = pk.encode_to_canonical_key()?;

        match self.tree.delete(self.buffer_pool, key)? {
            Some(rid) => {
                let bytes = self.record_store.get(self.buffer_pool, rid)?;
                self.record_store.delete(self.buffer_pool, rid)?;
                self.sync_metadata_if_needed()?;
                match bytes {
                    Some(b) => {
                        let row = self.metadata.schema.decode_tuple(&b)?;
                        Ok(Some(row))
                    }
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }

    /// Performs an ordered range scan over primary keys in `[start, end)`.
    ///
    /// - `start = None` begins from the lowest key.
    /// - `end = None` scans up to the highest key.
    ///
    /// Returns rows ordered monotonically by primary key.
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

    /// Validates the structural invariants of the table's underlying B+ Tree.
    pub fn validate_tree(&mut self) -> Result<()> {
        self.tree.validate_tree(self.buffer_pool)
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
            crate::engine::catalog::Catalog::update_table_metadata(
                self.buffer_pool,
                &self.metadata,
            )?;
        }

        Ok(())
    }
}
