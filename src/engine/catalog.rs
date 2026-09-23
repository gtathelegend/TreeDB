use std::collections::HashSet;

use crate::engine::types::{Column, DataType, Schema, MAX_COLUMN_NAME_LEN};
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::index::leaf::LeafNode;
use crate::storage::buffer_pool::BufferPool;
use crate::storage::metadata::{DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID};
use crate::storage::overflow_page::{OverflowPage, NO_OVERFLOW_PAGE};
use crate::storage::page::{Page, PageId, PageType, PAGE_PAYLOAD_SIZE};
use crate::storage::record_page::{OverflowDescriptor, RecordPage};

/// Recognizable magic bytes for Catalog pages.
pub const CATALOG_MAGIC: &[u8; 4] = b"CATL";

/// Format version 1 for Catalog page layout.
pub const CURRENT_CATALOG_VERSION: u32 = 1;

/// Maximum allowable length for a table name in bytes.
pub const MAX_TABLE_NAME_LEN: usize = 64;

/// Size in bytes of the catalog page header within the payload (32 bytes).
///
/// Layout:
/// - `0..4`: `magic: [u8; 4]` (`CATL`)
/// - `4..8`: `format_version: u32` (1)
/// - `8..12`: `next_table_id: u32` (Monotonically increasing ID counter, stored on root catalog page)
/// - `12..16`: `table_count: u32` (Number of active table entries on this page)
/// - `16..20`: `next_catalog_page_id: PageId` (0 if no next page)
/// - `20..32`: `reserved: [u8; 12]`
pub const CATALOG_HEADER_SIZE: usize = 32;

/// Fixed-size header for each serialized table entry in the catalog payload (26 bytes).
///
/// Layout:
/// - `0..4`: `table_id: u32`
/// - `4..8`: `schema_version: u32`
/// - `8..12`: `root_page_id: u32`
/// - `12..16`: `record_page_id: u32`
/// - `16..20`: `flags: u32`
/// - `20..22`: `primary_key_idx: u16`
/// - `22..24`: `column_count: u16`
/// - `24..26`: `name_len: u16`
pub const TABLE_ENTRY_HEADER_SIZE: usize = 26;

/// Available payload capacity for catalog entries on a single page.
pub const CATALOG_ENTRIES_CAPACITY: usize = PAGE_PAYLOAD_SIZE - CATALOG_HEADER_SIZE;

/// Sentinel indicating that no next catalog page exists in the chain.
pub const NO_NEXT_CATALOG_PAGE: PageId = 0;

/// Unique, stable, logical identifier for a database table.
///
/// Table IDs are allocated monotonically and are never reused upon table drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableId(pub u32);

impl std::fmt::Display for TableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TableId({})", self.0)
    }
}

/// Persistent metadata describing a logical database table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMetadata {
    /// Unique, stable logical table identifier.
    pub table_id: TableId,
    /// UTF-8 table name.
    pub name: String,
    /// Schema definition version (defaults to 1).
    pub schema_version: u32,
    /// Root page ID of the table's primary B+ Tree index.
    pub root_page_id: PageId,
    /// Initial/active record page ID of the table's slotted RecordStore.
    pub record_page_id: PageId,
    /// Table configuration flags (reserved for future schema options).
    pub flags: u32,
    /// Relational schema defining columns and the primary key.
    pub schema: Schema,
}

impl TableMetadata {
    /// Computes the serialized byte size of this table metadata entry.
    pub fn byte_size(&self) -> usize {
        TABLE_ENTRY_HEADER_SIZE
            + self.name.len()
            + self
                .schema
                .columns
                .iter()
                .map(|col| 4 + col.name.len())
                .sum::<usize>()
    }
}

/// Validates table name syntax and constraints.
///
/// Rules:
/// - Non-empty
/// - Maximum 64 bytes
/// - Valid UTF-8
pub fn validate_table_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidTableName(
            "table name cannot be empty".to_string(),
        ));
    }
    if name.len() > MAX_TABLE_NAME_LEN {
        return Err(Error::InvalidTableName(format!(
            "table name length {} exceeds limit of {} bytes",
            name.len(),
            MAX_TABLE_NAME_LEN
        )));
    }
    Ok(())
}

/// In-memory representation of an individual 4096-byte Catalog Page (`PageType::Catalog` or `PageType::Metadata`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPage {
    /// Physical page identifier.
    pub page_id: PageId,
    /// Format version of the catalog layout.
    pub format_version: u32,
    /// Monotonically increasing next table ID counter (active on root catalog page).
    pub next_table_id: u32,
    /// Next chained catalog page ID (0 if last page in chain).
    pub next_catalog_page_id: PageId,
    /// Active table metadata entries stored on this page.
    pub entries: Vec<TableMetadata>,
}

impl CatalogPage {
    /// Creates a newly initialized, empty catalog page with default headers.
    pub fn new(page_id: PageId) -> Self {
        Self {
            page_id,
            format_version: CURRENT_CATALOG_VERSION,
            next_table_id: 1,
            next_catalog_page_id: NO_NEXT_CATALOG_PAGE,
            entries: Vec::new(),
        }
    }

    /// Computes the total serialized byte size of all entries on this page.
    pub fn entries_byte_size(&self) -> usize {
        self.entries.iter().map(|e| e.byte_size()).sum()
    }

    /// Returns `true` if a new table entry with the given byte size can fit on this page.
    pub fn can_fit(&self, entry_size: usize) -> bool {
        self.entries_byte_size() + entry_size <= CATALOG_ENTRIES_CAPACITY
    }

    /// Serializes this `CatalogPage` into the payload region of a standard `Page`.
    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        let payload = page.payload_mut();

        // 1. Write catalog header (32 bytes)
        payload[0..4].copy_from_slice(CATALOG_MAGIC);
        payload[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        payload[8..12].copy_from_slice(&self.next_table_id.to_le_bytes());
        payload[12..16].copy_from_slice(&(self.entries.len() as u32).to_le_bytes());
        payload[16..20].copy_from_slice(&self.next_catalog_page_id.to_le_bytes());
        payload[20..32].fill(0); // reserved

        // 2. Write table entries
        let mut offset = CATALOG_HEADER_SIZE;
        for entry in &self.entries {
            let entry_len = entry.byte_size();

            if offset + entry_len > PAGE_PAYLOAD_SIZE {
                return Err(Error::CatalogCorruption(format!(
                    "catalog page {} overflowed payload capacity",
                    self.page_id
                )));
            }

            let name_bytes = entry.name.as_bytes();
            let name_len = name_bytes.len();
            let col_count = entry.schema.columns.len();

            payload[offset..offset + 4].copy_from_slice(&entry.table_id.0.to_le_bytes());
            payload[offset + 4..offset + 8].copy_from_slice(&entry.schema_version.to_le_bytes());
            payload[offset + 8..offset + 12].copy_from_slice(&entry.root_page_id.to_le_bytes());
            payload[offset + 12..offset + 16].copy_from_slice(&entry.record_page_id.to_le_bytes());
            payload[offset + 16..offset + 20].copy_from_slice(&entry.flags.to_le_bytes());
            payload[offset + 20..offset + 22]
                .copy_from_slice(&(entry.schema.primary_key_idx as u16).to_le_bytes());
            payload[offset + 22..offset + 24].copy_from_slice(&(col_count as u16).to_le_bytes());
            payload[offset + 24..offset + 26].copy_from_slice(&(name_len as u16).to_le_bytes());

            let mut cur = offset + TABLE_ENTRY_HEADER_SIZE;
            payload[cur..cur + name_len].copy_from_slice(name_bytes);
            cur += name_len;

            for col in &entry.schema.columns {
                payload[cur] = col.data_type.to_u8();
                payload[cur + 1] = if col.nullable { 1 } else { 0 };
                let col_name_bytes = col.name.as_bytes();
                let col_name_len = col_name_bytes.len();
                payload[cur + 2..cur + 4].copy_from_slice(&(col_name_len as u16).to_le_bytes());
                cur += 4;
                payload[cur..cur + col_name_len].copy_from_slice(col_name_bytes);
                cur += col_name_len;
            }

            offset += entry_len;
        }

        // Zero remaining payload to prevent stale byte leakage
        if offset < PAGE_PAYLOAD_SIZE {
            payload[offset..PAGE_PAYLOAD_SIZE].fill(0);
        }

        Ok(())
    }

    /// Deserializes a `CatalogPage` from a standard `Page`, validating structural invariants.
    pub fn deserialize(page: &Page) -> Result<Self> {
        let payload = page.payload();

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&payload[0..4]);

        // Support uninitialized/zeroed metadata page on brand-new database files
        if magic == [0, 0, 0, 0] {
            return Ok(Self::new(page.id()));
        }

        if &magic != CATALOG_MAGIC {
            return Err(Error::CatalogCorruption(format!(
                "invalid catalog magic on page {}: expected {:?}, found {:?}",
                page.id(),
                CATALOG_MAGIC,
                magic
            )));
        }

        let format_version = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        if format_version != CURRENT_CATALOG_VERSION {
            return Err(Error::CatalogVersionUnsupported {
                found: format_version,
                supported: CURRENT_CATALOG_VERSION,
            });
        }

        let next_table_id = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        let table_count =
            u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]) as usize;
        let next_catalog_page_id =
            u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);

        let mut entries = Vec::with_capacity(table_count);
        let mut offset = CATALOG_HEADER_SIZE;

        for _ in 0..table_count {
            if offset + TABLE_ENTRY_HEADER_SIZE > PAGE_PAYLOAD_SIZE {
                return Err(Error::CatalogCorruption(format!(
                    "unexpected end of payload reading table entry header on page {}",
                    page.id()
                )));
            }

            let table_id = u32::from_le_bytes([
                payload[offset],
                payload[offset + 1],
                payload[offset + 2],
                payload[offset + 3],
            ]);
            let schema_version = u32::from_le_bytes([
                payload[offset + 4],
                payload[offset + 5],
                payload[offset + 6],
                payload[offset + 7],
            ]);
            let root_page_id = u32::from_le_bytes([
                payload[offset + 8],
                payload[offset + 9],
                payload[offset + 10],
                payload[offset + 11],
            ]);
            let record_page_id = u32::from_le_bytes([
                payload[offset + 12],
                payload[offset + 13],
                payload[offset + 14],
                payload[offset + 15],
            ]);
            let flags = u32::from_le_bytes([
                payload[offset + 16],
                payload[offset + 17],
                payload[offset + 18],
                payload[offset + 19],
            ]);
            let primary_key_idx =
                u16::from_le_bytes([payload[offset + 20], payload[offset + 21]]) as usize;
            let column_count =
                u16::from_le_bytes([payload[offset + 22], payload[offset + 23]]) as usize;
            let name_len =
                u16::from_le_bytes([payload[offset + 24], payload[offset + 25]]) as usize;

            if name_len == 0 || name_len > MAX_TABLE_NAME_LEN {
                return Err(Error::CatalogCorruption(format!(
                    "invalid table name length {} on page {}",
                    name_len,
                    page.id()
                )));
            }

            let mut cur = offset + TABLE_ENTRY_HEADER_SIZE;
            if cur + name_len > PAGE_PAYLOAD_SIZE {
                return Err(Error::CatalogCorruption(format!(
                    "unexpected end of payload reading table name on page {}",
                    page.id()
                )));
            }

            let name_bytes = &payload[cur..cur + name_len];
            let name = std::str::from_utf8(name_bytes)
                .map_err(|_| {
                    Error::CatalogCorruption(format!(
                        "invalid UTF-8 in table name on page {}",
                        page.id()
                    ))
                })?
                .to_string();
            cur += name_len;

            let mut columns = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                if cur + 4 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::CatalogCorruption(format!(
                        "unexpected end of payload reading column header on page {}",
                        page.id()
                    )));
                }

                let data_type = DataType::from_u8(payload[cur])?;
                let nullable = match payload[cur + 1] {
                    0 => false,
                    1 => true,
                    other => {
                        return Err(Error::CatalogCorruption(format!(
                            "invalid nullable tag {other} on page {}",
                            page.id()
                        )));
                    }
                };
                let col_name_len =
                    u16::from_le_bytes([payload[cur + 2], payload[cur + 3]]) as usize;
                cur += 4;

                if col_name_len == 0 || col_name_len > MAX_COLUMN_NAME_LEN {
                    return Err(Error::CatalogCorruption(format!(
                        "invalid column name length {col_name_len} on page {}",
                        page.id()
                    )));
                }

                if cur + col_name_len > PAGE_PAYLOAD_SIZE {
                    return Err(Error::CatalogCorruption(format!(
                        "unexpected end of payload reading column name on page {}",
                        page.id()
                    )));
                }

                let col_name = std::str::from_utf8(&payload[cur..cur + col_name_len])
                    .map_err(|_| {
                        Error::CatalogCorruption(format!(
                            "invalid UTF-8 in column name on page {}",
                            page.id()
                        ))
                    })?
                    .to_string();
                cur += col_name_len;

                columns.push(Column::new(col_name, data_type, nullable));
            }

            let schema = Schema::new(columns, primary_key_idx)?;

            entries.push(TableMetadata {
                table_id: TableId(table_id),
                name,
                schema_version,
                root_page_id,
                record_page_id,
                flags,
                schema,
            });

            offset = cur;
        }

        Ok(Self {
            page_id: page.id(),
            format_version,
            next_table_id,
            next_catalog_page_id,
            entries,
        })
    }

    /// Searches this page for a table metadata entry matching `name`.
    pub fn find_by_name(&self, name: &str) -> Option<&TableMetadata> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Searches this page for a table metadata entry matching `table_id`.
    pub fn find_by_id(&self, table_id: TableId) -> Option<&TableMetadata> {
        self.entries.iter().find(|e| e.table_id == table_id)
    }

    /// Adds a new table metadata entry to this page.
    pub fn add_entry(&mut self, entry: TableMetadata) -> Result<()> {
        if !self.can_fit(entry.byte_size()) {
            return Err(Error::CatalogCorruption(format!(
                "catalog page {} is full",
                self.page_id
            )));
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Removes a table metadata entry matching `name` from this page.
    pub fn remove_entry(&mut self, name: &str) -> Option<TableMetadata> {
        if let Some(pos) = self.entries.iter().position(|e| e.name == name) {
            Some(self.entries.remove(pos))
        } else {
            None
        }
    }
}

/// Catalog manager coordinating table schema metadata across persistent catalog pages.
pub struct Catalog;

impl Catalog {
    /// Creates a new table in the catalog and initializes its physical structures.
    ///
    /// # Process
    /// 1. Validates table name and schema invariants.
    /// 2. Verifies table name uniqueness across all catalog pages.
    /// 3. Reads root catalog page (Page 1) to allocate a new monotonic `TableId`.
    /// 4. Allocates initial physical structures (`PageType::Leaf` and `PageType::Record`).
    /// 5. Appends `TableMetadata` to an existing catalog page or a newly allocated chained catalog page.
    ///
    /// # Errors
    /// * `Error::TableAlreadyExists` if a table with the same name already exists.
    /// * `Error::InvalidTableName` if the table name is invalid.
    /// * `Error::TableSchemaMismatch` if the schema is invalid.
    pub fn create_table(
        buffer_pool: &mut BufferPool,
        name: &str,
        schema: Schema,
    ) -> Result<TableId> {
        validate_table_name(name)?;
        schema.validate()?;

        // 1. Check for duplicate table name across all catalog pages
        if Self::get_table(buffer_pool, name)?.is_some() {
            return Err(Error::TableAlreadyExists(name.to_string()));
        }

        // 2. Read root catalog page to get and increment next_table_id
        let root_cat_page_id = DEFAULT_METADATA_PAGE_ID;
        let mut root_cat = {
            let page = buffer_pool.fetch_page(root_cat_page_id)?;
            let cat = CatalogPage::deserialize(page)?;
            buffer_pool.unpin_page(root_cat_page_id, false)?;
            cat
        };

        let new_table_id = TableId(root_cat.next_table_id);
        root_cat.next_table_id += 1;

        // 3. Allocate physical structures for the new table
        // A: B+ Tree Root (initialized as an empty Leaf node)
        let root_page_id = buffer_pool.allocate_page(PageType::Leaf)?;
        let leaf = LeafNode::new();
        {
            let leaf_page_mut = buffer_pool.get_page_mut(root_page_id)?;
            leaf.serialize(leaf_page_mut)?;
        }
        buffer_pool.unpin_page(root_page_id, true)?;

        // B: Slotted Record Store Page (initialized as an empty RecordPage)
        let record_page_id = buffer_pool.allocate_page(PageType::Record)?;
        let rec_page = RecordPage::new();
        {
            let rec_page_mut = buffer_pool.get_page_mut(record_page_id)?;
            rec_page.serialize(rec_page_mut)?;
        }
        buffer_pool.unpin_page(record_page_id, true)?;

        let metadata = TableMetadata {
            table_id: new_table_id,
            name: name.to_string(),
            schema_version: 1,
            root_page_id,
            record_page_id,
            flags: 0,
            schema,
        };

        // 4. Insert metadata entry into catalog page chain
        let entry_size = metadata.byte_size();
        if root_cat.can_fit(entry_size) {
            root_cat.add_entry(metadata)?;
            let page_mut = buffer_pool.fetch_page_mut(root_cat_page_id)?;
            root_cat.serialize(page_mut)?;
            buffer_pool.unpin_page(root_cat_page_id, true)?;
        } else {
            // Save updated next_table_id on root catalog page
            {
                let page_mut = buffer_pool.fetch_page_mut(root_cat_page_id)?;
                root_cat.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(root_cat_page_id, true)?;

            // Traverse chain to find a page with room or append a new page
            let mut curr_pid = root_cat.next_catalog_page_id;
            let mut prev_pid = root_cat_page_id;
            let mut inserted = false;

            while curr_pid != NO_NEXT_CATALOG_PAGE {
                let mut cat = {
                    let page = buffer_pool.fetch_page(curr_pid)?;
                    let c = CatalogPage::deserialize(page)?;
                    buffer_pool.unpin_page(curr_pid, false)?;
                    c
                };

                if cat.can_fit(entry_size) {
                    cat.add_entry(metadata.clone())?;
                    let page_mut = buffer_pool.fetch_page_mut(curr_pid)?;
                    cat.serialize(page_mut)?;
                    buffer_pool.unpin_page(curr_pid, true)?;
                    inserted = true;
                    break;
                }

                prev_pid = curr_pid;
                curr_pid = cat.next_catalog_page_id;
            }

            if !inserted {
                // Allocate a new catalog page and link it
                let new_cat_pid = buffer_pool.allocate_page(PageType::Catalog)?;
                let mut new_cat = CatalogPage::new(new_cat_pid);
                new_cat.add_entry(metadata)?;

                {
                    let new_page_mut = buffer_pool.get_page_mut(new_cat_pid)?;
                    new_cat.serialize(new_page_mut)?;
                }
                buffer_pool.unpin_page(new_cat_pid, true)?;

                // Link previous catalog page to new_cat_pid
                let mut prev_cat = {
                    let page = buffer_pool.fetch_page(prev_pid)?;
                    let c = CatalogPage::deserialize(page)?;
                    buffer_pool.unpin_page(prev_pid, false)?;
                    c
                };
                prev_cat.next_catalog_page_id = new_cat_pid;
                {
                    let prev_page_mut = buffer_pool.fetch_page_mut(prev_pid)?;
                    prev_cat.serialize(prev_page_mut)?;
                }
                buffer_pool.unpin_page(prev_pid, true)?;
            }
        }

        Ok(new_table_id)
    }

    /// Drops a table from the catalog and reclaims all of its physical pages into the freelist.
    ///
    /// # Process
    /// 1. Finds table metadata by name (returns `Error::TableNotFound` if absent).
    /// 2. Traverses and collects all B+ Tree node pages belonging to the table.
    /// 3. Traverses and collects the table's record page and any referenced overflow chains.
    /// 4. Reclaims every collected physical page into the persistent freelist via `BufferPool::free_page`.
    /// 5. Removes the metadata entry from the catalog page.
    pub fn drop_table(buffer_pool: &mut BufferPool, name: &str) -> Result<()> {
        validate_table_name(name)?;

        // 1. Locate the table entry across catalog pages
        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        let mut target_metadata: Option<TableMetadata> = None;
        let mut target_cat_pid = 0;

        while curr_pid != NO_NEXT_CATALOG_PAGE {
            let cat = {
                let page = buffer_pool.fetch_page(curr_pid)?;
                let c = CatalogPage::deserialize(page)?;
                buffer_pool.unpin_page(curr_pid, false)?;
                c
            };

            if let Some(entry) = cat.find_by_name(name) {
                target_metadata = Some(entry.clone());
                target_cat_pid = curr_pid;
                break;
            }

            curr_pid = cat.next_catalog_page_id;
        }

        let metadata = match target_metadata {
            Some(m) => m,
            None => return Err(Error::TableNotFound(name.to_string())),
        };

        // 2. Reclaim physical B+ Tree pages
        let tree = BPlusTree::from_root(metadata.root_page_id);
        let tree_pages = tree.collect_all_pages(buffer_pool)?;
        for pid in tree_pages {
            buffer_pool.free_page(pid)?;
        }

        // 3. Reclaim physical RecordStore pages and overflow chains
        if metadata.record_page_id != 0 {
            let rec_pid = metadata.record_page_id;
            // Scan record page for any overflow descriptors
            let mut overflow_heads = Vec::new();
            {
                let page = buffer_pool.fetch_page(rec_pid)?;
                if let Ok(rec_page) = RecordPage::deserialize(page) {
                    for slot in &rec_page.slots {
                        if slot.is_overflow() {
                            let off = slot.offset as usize;
                            let payload = page.payload();
                            if off + 8 <= payload.len() {
                                let head_pid = u32::from_le_bytes([
                                    payload[off],
                                    payload[off + 1],
                                    payload[off + 2],
                                    payload[off + 3],
                                ]);
                                let total_len = u32::from_le_bytes([
                                    payload[off + 4],
                                    payload[off + 5],
                                    payload[off + 6],
                                    payload[off + 7],
                                ]);
                                let desc = OverflowDescriptor {
                                    head_page_id: head_pid,
                                    total_length: total_len,
                                };
                                overflow_heads.push(desc.head_page_id);
                            }
                        }
                    }
                }
                buffer_pool.unpin_page(rec_pid, false)?;
            }

            // Free overflow chains
            for head_pid in overflow_heads {
                let mut curr_ovf = head_pid;
                let mut visited = HashSet::new();
                while curr_ovf != NO_OVERFLOW_PAGE {
                    if !visited.insert(curr_ovf) {
                        break;
                    }
                    let next_ovf = {
                        let page = buffer_pool.fetch_page(curr_ovf)?;
                        let next = match OverflowPage::deserialize(page) {
                            Ok(ovf) => ovf.next_page_id,
                            Err(_) => NO_OVERFLOW_PAGE,
                        };
                        buffer_pool.unpin_page(curr_ovf, false)?;
                        next
                    };
                    buffer_pool.free_page(curr_ovf)?;
                    curr_ovf = next_ovf;
                }
            }

            // Free record page itself
            buffer_pool.free_page(rec_pid)?;
        }

        // 4. Remove entry from catalog page
        let mut target_cat = {
            let page = buffer_pool.fetch_page(target_cat_pid)?;
            let c = CatalogPage::deserialize(page)?;
            buffer_pool.unpin_page(target_cat_pid, false)?;
            c
        };
        target_cat.remove_entry(name);
        {
            let page_mut = buffer_pool.fetch_page_mut(target_cat_pid)?;
            target_cat.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(target_cat_pid, true)?;

        Ok(())
    }

    /// Looks up a table in the catalog by name.
    ///
    /// Returns `Ok(Some(TableMetadata))` if found, or `Ok(None)` if absent.
    pub fn get_table(buffer_pool: &mut BufferPool, name: &str) -> Result<Option<TableMetadata>> {
        validate_table_name(name)?;

        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        let mut visited = HashSet::new();

        while curr_pid != NO_NEXT_CATALOG_PAGE {
            if !visited.insert(curr_pid) {
                return Err(Error::CatalogCorruption(format!(
                    "cycle detected in catalog page chain at page {curr_pid}"
                )));
            }

            let page = buffer_pool.fetch_page(curr_pid)?;
            let cat = match CatalogPage::deserialize(page) {
                Ok(c) => c,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(curr_pid, false)?;

            if let Some(entry) = cat.find_by_name(name) {
                return Ok(Some(entry.clone()));
            }

            curr_pid = cat.next_catalog_page_id;
        }

        Ok(None)
    }

    /// Updates the metadata entry for an existing table in the catalog.
    pub fn update_table_metadata(
        buffer_pool: &mut BufferPool,
        metadata: &TableMetadata,
    ) -> Result<()> {
        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        while curr_pid != NO_NEXT_CATALOG_PAGE {
            let mut cat = {
                let page = buffer_pool.fetch_page(curr_pid)?;
                let c = CatalogPage::deserialize(page)?;
                buffer_pool.unpin_page(curr_pid, false)?;
                c
            };

            if let Some(pos) = cat
                .entries
                .iter()
                .position(|e| e.table_id == metadata.table_id)
            {
                cat.entries[pos] = metadata.clone();
                let page_mut = buffer_pool.fetch_page_mut(curr_pid)?;
                cat.serialize(page_mut)?;
                buffer_pool.unpin_page(curr_pid, true)?;
                return Ok(());
            }

            curr_pid = cat.next_catalog_page_id;
        }

        Err(Error::TableNotFound(metadata.name.clone()))
    }

    /// Looks up a table in the catalog by `TableId`.
    ///
    /// Returns `Ok(Some(TableMetadata))` if found, or `Ok(None)` if absent.
    pub fn get_table_by_id(
        buffer_pool: &mut BufferPool,
        table_id: TableId,
    ) -> Result<Option<TableMetadata>> {
        if table_id.0 == 0 {
            return Err(Error::InvalidTableId(0));
        }

        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        let mut visited = HashSet::new();

        while curr_pid != NO_NEXT_CATALOG_PAGE {
            if !visited.insert(curr_pid) {
                return Err(Error::CatalogCorruption(format!(
                    "cycle detected in catalog page chain at page {curr_pid}"
                )));
            }

            let page = buffer_pool.fetch_page(curr_pid)?;
            let cat = match CatalogPage::deserialize(page) {
                Ok(c) => c,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(curr_pid, false)?;

            if let Some(entry) = cat.find_by_id(table_id) {
                return Ok(Some(entry.clone()));
            }

            curr_pid = cat.next_catalog_page_id;
        }

        Ok(None)
    }

    /// Returns a list of all tables currently registered in the catalog.
    pub fn list_tables(buffer_pool: &mut BufferPool) -> Result<Vec<TableMetadata>> {
        let mut tables = Vec::new();
        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        let mut visited = HashSet::new();

        while curr_pid != NO_NEXT_CATALOG_PAGE {
            if !visited.insert(curr_pid) {
                return Err(Error::CatalogCorruption(format!(
                    "cycle detected in catalog page chain at page {curr_pid}"
                )));
            }

            let page = buffer_pool.fetch_page(curr_pid)?;
            let cat = match CatalogPage::deserialize(page) {
                Ok(c) => c,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(curr_pid, false)?;

            tables.extend(cat.entries);
            curr_pid = cat.next_catalog_page_id;
        }

        Ok(tables)
    }

    /// Validates the structural integrity and referential correctness of the catalog.
    ///
    /// # Checks
    /// 1. Catalog page chain contains no cycles.
    /// 2. Every catalog page has a valid `PageType::Catalog` or `PageType::Metadata`.
    /// 3. Magic bytes and format versions match supported constants.
    /// 4. All `TableId`s are strictly positive and less than `next_table_id`.
    /// 5. No duplicate `TableId`s or table names exist.
    /// 6. All referenced `root_page_id` and `record_page_id` pointers:
    ///    - Are within the database page count bounds.
    ///    - Are not reserved pages (Page 0 or Page 1).
    ///    - Have valid expected page types (`PageType::Leaf`/`PageType::Internal` and `PageType::Record`).
    pub fn validate_catalog(buffer_pool: &mut BufferPool) -> Result<()> {
        let page_count = buffer_pool.page_manager().page_count();
        let mut curr_pid = DEFAULT_METADATA_PAGE_ID;
        let mut visited_pages = HashSet::new();
        let mut seen_ids = HashSet::new();
        let mut seen_names = HashSet::new();

        let mut max_table_id_counter: Option<u32> = None;

        while curr_pid != NO_NEXT_CATALOG_PAGE {
            if !visited_pages.insert(curr_pid) {
                return Err(Error::CatalogCorruption(format!(
                    "cycle detected in catalog page chain at page {curr_pid}"
                )));
            }

            if curr_pid >= page_count {
                return Err(Error::CatalogCorruption(format!(
                    "catalog page {curr_pid} exceeds database page count {page_count}"
                )));
            }

            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();
            if page_type != PageType::Metadata && page_type != PageType::Catalog {
                let _ = buffer_pool.unpin_page(curr_pid, false);
                return Err(Error::CatalogCorruption(format!(
                    "catalog page {curr_pid} has invalid page type {page_type:?}"
                )));
            }

            let cat = match CatalogPage::deserialize(page) {
                Ok(c) => c,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(curr_pid, false)?;

            if max_table_id_counter.is_none() {
                max_table_id_counter = Some(cat.next_table_id);
            }

            let max_counter = max_table_id_counter.unwrap();

            for entry in &cat.entries {
                // Table ID validation
                if entry.table_id.0 == 0 || entry.table_id.0 >= max_counter {
                    return Err(Error::CatalogCorruption(format!(
                        "invalid table ID {} (next_table_id counter is {})",
                        entry.table_id.0, max_counter
                    )));
                }

                if !seen_ids.insert(entry.table_id.0) {
                    return Err(Error::CatalogCorruption(format!(
                        "duplicate table ID {} detected in catalog",
                        entry.table_id.0
                    )));
                }

                // Table name and schema validation
                validate_table_name(&entry.name)?;
                entry.schema.validate()?;
                if !seen_names.insert(entry.name.clone()) {
                    return Err(Error::CatalogCorruption(format!(
                        "duplicate table name '{}' detected in catalog",
                        entry.name
                    )));
                }

                // Referential validation for root_page_id
                if entry.root_page_id >= page_count
                    || entry.root_page_id == HEADER_PAGE_ID
                    || entry.root_page_id == DEFAULT_METADATA_PAGE_ID
                {
                    return Err(Error::CatalogCorruption(format!(
                        "table '{}' has invalid root_page_id {}",
                        entry.name, entry.root_page_id
                    )));
                }

                let root_page = buffer_pool.fetch_page(entry.root_page_id)?;
                let root_type = root_page.page_type();
                buffer_pool.unpin_page(entry.root_page_id, false)?;
                if root_type != PageType::Leaf && root_type != PageType::Internal {
                    return Err(Error::CatalogCorruption(format!(
                        "table '{}' root page {} has unexpected page type {:?}",
                        entry.name, entry.root_page_id, root_type
                    )));
                }

                // Referential validation for record_page_id
                if entry.record_page_id != 0 {
                    if entry.record_page_id >= page_count
                        || entry.record_page_id == HEADER_PAGE_ID
                        || entry.record_page_id == DEFAULT_METADATA_PAGE_ID
                    {
                        return Err(Error::CatalogCorruption(format!(
                            "table '{}' has invalid record_page_id {}",
                            entry.name, entry.record_page_id
                        )));
                    }

                    let rec_page = buffer_pool.fetch_page(entry.record_page_id)?;
                    let rec_type = rec_page.page_type();
                    buffer_pool.unpin_page(entry.record_page_id, false)?;
                    if rec_type != PageType::Record {
                        return Err(Error::CatalogCorruption(format!(
                            "table '{}' record page {} has unexpected page type {:?}",
                            entry.name, entry.record_page_id, rec_type
                        )));
                    }
                }
            }

            curr_pid = cat.next_catalog_page_id;
        }

        Ok(())
    }
}
