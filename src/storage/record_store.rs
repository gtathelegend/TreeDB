use std::collections::HashSet;

use crate::engine::isolation::{CommitSeq, MvccVisibilityPolicy, Snapshot};
use crate::error::{Error, Result};
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::metadata::is_reserved_page;
use crate::storage::overflow_page::{OverflowPage, NO_OVERFLOW_PAGE, OVERFLOW_PAYLOAD_SIZE};
use crate::storage::page::{PageId, PageType};
use crate::storage::record_page::{
    RecordLocation, RecordPage, MAX_RECORD_SIZE, OVERFLOW_DESCRIPTOR_SIZE,
};
use crate::wal::lsn::TxnId;

/// Canonical MVCC on-disk version serialization format version.
pub const MVCC_VERSION_FORMAT: u16 = 1;

/// Fixed header size in bytes for persistent MVCC record versions.
///
/// Layout:
/// - `format_version`: u16 (2B)
/// - `xmin`: u64 (8B)
/// - `has_xmax`: u8 (1B)
/// - `xmax`: u64 (8B)
/// - `has_created_at`: u8 (1B)
/// - `created_at`: u64 (8B)
/// - `has_deleted_at`: u8 (1B)
/// - `deleted_at`: u64 (8B)
/// - `has_prev`: u8 (1B)
/// - `prev_page_id`: u32 (4B)
/// - `prev_slot_id`: u32 (4B)
///
/// Total = 46 bytes.
pub const VERSION_HEADER_SIZE: usize = 46;

/// Maximum allowable version chain traversal depth before rejecting as pathological.
pub const MAX_VERSION_CHAIN_DEPTH: usize = 1000;

/// Strongly-typed identifier representing a specific physical version of a record.
///
/// Distinct from logical record identity (which in TreeDB is established by the table's
/// primary key in the primary B+ Tree). Each physical version allocated in `RecordStore`
/// has a unique `VersionId` corresponding to its physical `RecordId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionId(pub RecordId);

impl VersionId {
    /// Creates a new `VersionId` wrapping a physical `RecordId`.
    pub const fn new(page_id: PageId, slot_id: u32) -> Self {
        Self(RecordId::new(page_id, slot_id))
    }

    /// Returns the underlying `RecordId`.
    pub const fn record_id(&self) -> RecordId {
        self.0
    }
}

impl From<RecordId> for VersionId {
    fn from(rid: RecordId) -> Self {
        Self(rid)
    }
}

impl From<VersionId> for RecordId {
    fn from(vid: VersionId) -> Self {
        vid.0
    }
}

/// Persistent MVCC record version metadata describing creation, deletion, and version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMetadata {
    pub format_version: u16,
    pub xmin: TxnId,
    pub xmax: Option<TxnId>,
    pub created_at: Option<CommitSeq>,
    pub deleted_at: Option<CommitSeq>,
    pub prev_version: Option<RecordId>,
}

impl VersionMetadata {
    /// Creates a new uncommitted version created by `xmin`, optionally chained from `prev_version`.
    pub fn new(xmin: TxnId, prev_version: Option<RecordId>) -> Self {
        Self {
            format_version: MVCC_VERSION_FORMAT,
            xmin,
            xmax: None,
            created_at: None,
            deleted_at: None,
            prev_version,
        }
    }

    /// Creates a new committed version created by `xmin` with `created_at` sequence.
    pub fn new_committed(
        xmin: TxnId,
        created_at: CommitSeq,
        prev_version: Option<RecordId>,
    ) -> Self {
        Self {
            format_version: MVCC_VERSION_FORMAT,
            xmin,
            xmax: None,
            created_at: Some(created_at),
            deleted_at: None,
            prev_version,
        }
    }

    /// Serializes this version header into a slice of length at least `VERSION_HEADER_SIZE`.
    pub fn serialize_into(&self, buf: &mut [u8]) {
        assert!(
            buf.len() >= VERSION_HEADER_SIZE,
            "buffer must have at least {VERSION_HEADER_SIZE} bytes"
        );
        buf[0..2].copy_from_slice(&self.format_version.to_le_bytes());
        buf[2..10].copy_from_slice(&self.xmin.0.to_le_bytes());
        match self.xmax {
            Some(x) => {
                buf[10] = 1;
                buf[11..19].copy_from_slice(&x.0.to_le_bytes());
            }
            None => {
                buf[10] = 0;
                buf[11..19].copy_from_slice(&0u64.to_le_bytes());
            }
        }
        match self.created_at {
            Some(c) => {
                buf[19] = 1;
                buf[20..28].copy_from_slice(&c.0.to_le_bytes());
            }
            None => {
                buf[19] = 0;
                buf[20..28].copy_from_slice(&0u64.to_le_bytes());
            }
        }
        match self.deleted_at {
            Some(d) => {
                buf[28] = 1;
                buf[29..37].copy_from_slice(&d.0.to_le_bytes());
            }
            None => {
                buf[28] = 0;
                buf[29..37].copy_from_slice(&0u64.to_le_bytes());
            }
        }
        match self.prev_version {
            Some(rid) => {
                buf[37] = 1;
                buf[38..42].copy_from_slice(&rid.page_id.to_le_bytes());
                buf[42..46].copy_from_slice(&rid.slot_id.to_le_bytes());
            }
            None => {
                buf[37] = 0;
                buf[38..42].copy_from_slice(&0u32.to_le_bytes());
                buf[42..46].copy_from_slice(&0u32.to_le_bytes());
            }
        }
    }

    /// Deserializes a byte buffer into `VersionMetadata` and the remaining user payload.
    pub fn deserialize(buf: &[u8]) -> Result<(Self, &[u8])> {
        if buf.len() < VERSION_HEADER_SIZE {
            return Err(Error::InvalidVersionMetadata(format!(
                "record buffer length {} is smaller than version header size {VERSION_HEADER_SIZE}",
                buf.len()
            )));
        }

        let format_version = u16::from_le_bytes([buf[0], buf[1]]);
        if format_version != MVCC_VERSION_FORMAT {
            return Err(Error::InvalidVersionMetadata(format!(
                "unsupported version format version {format_version}, supported {MVCC_VERSION_FORMAT}"
            )));
        }

        let xmin = TxnId(u64::from_le_bytes(buf[2..10].try_into().unwrap()));
        let xmax = if buf[10] == 1 {
            Some(TxnId(u64::from_le_bytes(buf[11..19].try_into().unwrap())))
        } else {
            None
        };
        let created_at = if buf[19] == 1 {
            Some(CommitSeq(u64::from_le_bytes(
                buf[20..28].try_into().unwrap(),
            )))
        } else {
            None
        };
        let deleted_at = if buf[28] == 1 {
            Some(CommitSeq(u64::from_le_bytes(
                buf[29..37].try_into().unwrap(),
            )))
        } else {
            None
        };
        let prev_version = if buf[37] == 1 {
            let pid = u32::from_le_bytes(buf[38..42].try_into().unwrap());
            let sid = u32::from_le_bytes(buf[42..46].try_into().unwrap());
            Some(RecordId::new(pid, sid))
        } else {
            None
        };

        // Semantic invariant checks
        if let (Some(c), Some(d)) = (created_at, deleted_at) {
            if c > d {
                return Err(Error::InvalidVersionMetadata(format!(
                    "creation commit sequence {c} exceeds deletion commit sequence {d}"
                )));
            }
        }
        if deleted_at.is_some() && xmax.is_none() {
            return Err(Error::InvalidVersionMetadata(
                "version has deleted_at commit sequence without deleting transaction identity"
                    .into(),
            ));
        }

        let meta = Self {
            format_version,
            xmin,
            xmax,
            created_at,
            deleted_at,
            prev_version,
        };
        let payload = &buf[VERSION_HEADER_SIZE..];
        Ok((meta, payload))
    }
}

/// Persistent record storage manager coordinating variable-length inline and overflow records.
///
/// `RecordStore` allocates, reads, deletes, and compacts slotted records across
/// physical database pages via the `BufferPool`. Values $\le \text{MAX\_RECORD\_SIZE}$ (4070 bytes)
/// are stored inline in `PageType::Record` pages. Larger values are stored across singly-linked
/// chains of `PageType::Overflow` pages referenced by an on-page overflow descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordStore {
    /// ID of the most recently allocated or active record page.
    last_page_id: PageId,
}

impl Default for RecordStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordStore {
    /// Creates a new `RecordStore` with uninitialized active page pointer.
    pub const fn new() -> Self {
        Self {
            last_page_id: INVALID_PAGE_ID,
        }
    }

    /// Creates a `RecordStore` referencing a known active record page ID.
    pub const fn with_last_page(last_page_id: PageId) -> Self {
        Self { last_page_id }
    }

    /// Returns the ID of the current active record page, or `INVALID_PAGE_ID` (0).
    pub const fn last_page_id(&self) -> PageId {
        self.last_page_id
    }

    /// Inserts a variable-length record payload into the storage engine.
    ///
    /// Values $\le \text{MAX\_RECORD\_SIZE}$ (4070 bytes) are stored inline on a `PageType::Record` page.
    /// Values $> \text{MAX\_RECORD\_SIZE}$ are stored across a singly-linked chain of `PageType::Overflow` pages
    /// with an 8-byte descriptor installed on a `PageType::Record` page.
    ///
    /// # Returns
    /// * `Ok(RecordId)`: Stable logical record identifier pointing to the slot on the record page.
    pub fn insert(&mut self, buffer_pool: &mut BufferPool, record: &[u8]) -> Result<RecordId> {
        let record_len = record.len();

        // Path A: Inline Storage (<= MAX_RECORD_SIZE)
        if record_len <= MAX_RECORD_SIZE {
            let (target_page_id, mut record_page) =
                self.get_or_allocate_record_page(buffer_pool, record_len)?;
            let slot_id = record_page.insert(record)?;

            {
                let page_mut = buffer_pool.fetch_page_mut(target_page_id)?;
                record_page.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(target_page_id, true)?;
            buffer_pool.check_fault(
                crate::engine::failure::StorageFailurePoint::AfterRecordVersionAllocated,
            )?;

            return Ok(RecordId::new(target_page_id, slot_id));
        }

        // Path B: Multi-Page Overflow Storage (> MAX_RECORD_SIZE)
        let total_len = record_len;
        let overflow_pages_needed = total_len.div_ceil(OVERFLOW_PAYLOAD_SIZE);

        // 1. Allocate all required overflow pages from the BufferPool
        let mut allocated_pages = Vec::with_capacity(overflow_pages_needed);
        for _ in 0..overflow_pages_needed {
            buffer_pool.check_fault(
                crate::engine::failure::StorageFailurePoint::DuringOverflowPageAlloc,
            )?;
            let pid = buffer_pool.allocate_page(PageType::Overflow)?;
            buffer_pool.unpin_page(pid, true)?;
            allocated_pages.push(pid);
        }

        // 2. Populate and link each overflow page sequentially
        let mut offset = 0;
        for i in 0..overflow_pages_needed {
            let curr_pid = allocated_pages[i];
            let next_pid = if i + 1 < overflow_pages_needed {
                allocated_pages[i + 1]
            } else {
                NO_OVERFLOW_PAGE
            };

            let chunk_end = std::cmp::min(offset + OVERFLOW_PAYLOAD_SIZE, total_len);
            let chunk = &record[offset..chunk_end];
            offset = chunk_end;

            let op = OverflowPage::new(next_pid, chunk)?;
            {
                let page_mut = buffer_pool.fetch_page_mut(curr_pid)?;
                op.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(curr_pid, true)?;
        }

        let head_page_id = allocated_pages[0];

        // 3. Install 8-byte overflow descriptor on a slotted record page
        let (target_page_id, mut record_page) =
            self.get_or_allocate_record_page(buffer_pool, OVERFLOW_DESCRIPTOR_SIZE)?;
        let slot_id = record_page.insert_overflow_descriptor(head_page_id, total_len as u32)?;

        {
            let page_mut = buffer_pool.fetch_page_mut(target_page_id)?;
            record_page.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(target_page_id, true)?;
        buffer_pool.check_fault(
            crate::engine::failure::StorageFailurePoint::AfterRecordVersionAllocated,
        )?;

        Ok(RecordId::new(target_page_id, slot_id))
    }

    /// Internal helper to retrieve an active record page that can fit `required_space`,
    /// or allocate a new `PageType::Record` page.
    fn get_or_allocate_record_page(
        &mut self,
        buffer_pool: &mut BufferPool,
        required_space: usize,
    ) -> Result<(PageId, RecordPage)> {
        if self.last_page_id != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(self.last_page_id)?;
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(self.last_page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(self.last_page_id, false)?;

            if rp.can_fit(required_space) {
                Ok((self.last_page_id, rp))
            } else {
                let new_page_id = buffer_pool.allocate_page(PageType::Record)?;
                buffer_pool.unpin_page(new_page_id, true)?;
                self.last_page_id = new_page_id;
                Ok((new_page_id, RecordPage::new()))
            }
        } else {
            let new_page_id = buffer_pool.allocate_page(PageType::Record)?;
            buffer_pool.unpin_page(new_page_id, true)?;
            self.last_page_id = new_page_id;
            Ok((new_page_id, RecordPage::new()))
        }
    }

    /// Retrieves an owned copy of the record bytes associated with `record_id`.
    ///
    /// Transparently handles both inline payloads and multi-page overflow chains.
    ///
    /// # Pinning Discipline
    /// All accessed frames are promptly unpinned before returning, holding zero Buffer Pool pins on exit.
    pub fn get(
        &self,
        buffer_pool: &mut BufferPool,
        record_id: RecordId,
    ) -> Result<Option<Vec<u8>>> {
        if record_id.page_id == INVALID_PAGE_ID {
            return Ok(None);
        }

        let page = buffer_pool.fetch_page(record_id.page_id)?;
        if page.page_type() != PageType::Record {
            let _ = buffer_pool.unpin_page(record_id.page_id, false);
            return Err(Error::PageCorruption {
                page_id: record_id.page_id,
                reason: "expected Record page type tag",
            });
        }

        let record_page = match RecordPage::deserialize(page) {
            Ok(rp) => rp,
            Err(e) => {
                let _ = buffer_pool.unpin_page(record_id.page_id, false);
                return Err(e);
            }
        };

        let location = match record_page.get_location(record_id.slot_id) {
            Some(loc) => loc,
            None => {
                buffer_pool.unpin_page(record_id.page_id, false)?;
                return Ok(None);
            }
        };

        match location {
            RecordLocation::Inline(slice) => {
                let bytes = slice.to_vec();
                buffer_pool.unpin_page(record_id.page_id, false)?;
                Ok(Some(bytes))
            }
            RecordLocation::Overflow(descriptor) => {
                buffer_pool.unpin_page(record_id.page_id, false)?;
                let bytes = self.read_overflow_chain(
                    buffer_pool,
                    descriptor.head_page_id,
                    descriptor.total_length,
                )?;
                Ok(Some(bytes))
            }
        }
    }

    /// Reads and reassembles the complete byte vector across an overflow chain.
    fn read_overflow_chain(
        &self,
        buffer_pool: &mut BufferPool,
        head_page_id: PageId,
        expected_len: u32,
    ) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(expected_len as usize);
        let mut curr = head_page_id;
        let mut visited = HashSet::new();
        let total_pages = buffer_pool.page_count();

        while curr != NO_OVERFLOW_PAGE {
            if is_reserved_page(curr) {
                return Err(Error::OverflowCorruption(format!(
                    "overflow chain contains reserved page {curr}"
                )));
            }
            if curr >= total_pages {
                return Err(Error::OverflowCorruption(format!(
                    "overflow chain contains out-of-bounds page {curr} (total pages: {total_pages})"
                )));
            }
            if !visited.insert(curr) {
                return Err(Error::OverflowCorruption(format!(
                    "cycle detected in overflow chain at page {curr}"
                )));
            }

            let page = buffer_pool.fetch_page(curr)?;
            if page.page_type() != PageType::Overflow {
                let _ = buffer_pool.unpin_page(curr, false);
                return Err(Error::PageCorruption {
                    page_id: curr,
                    reason: "expected Overflow page type tag",
                });
            }

            let op = match OverflowPage::deserialize(page) {
                Ok(op) => op,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr, false);
                    return Err(e);
                }
            };

            let payload = op.payload().to_vec();
            let next = op.next_page_id();
            buffer_pool.unpin_page(curr, false)?;

            result.extend_from_slice(&payload);
            curr = next;
        }

        if result.len() != expected_len as usize {
            return Err(Error::OverflowCorruption(format!(
                "overflow chain length mismatch: reconstructed {} bytes, expected {expected_len}",
                result.len()
            )));
        }

        Ok(result)
    }

    /// Logically deletes the record associated with `record_id`.
    ///
    /// If the record is backed by an overflow chain, walks and frees all overflow pages,
    /// returning them to the persistent freelist.
    pub fn delete(&mut self, buffer_pool: &mut BufferPool, record_id: RecordId) -> Result<bool> {
        self.delete_and_count(buffer_pool, record_id)
            .map(|(deleted, _)| deleted)
    }

    /// Logically deletes the record associated with `record_id` and returns the number of
    /// overflow pages reclaimed into the freelist.
    pub fn delete_and_count(
        &mut self,
        buffer_pool: &mut BufferPool,
        record_id: RecordId,
    ) -> Result<(bool, u64)> {
        if record_id.page_id == INVALID_PAGE_ID {
            return Ok((false, 0));
        }

        let mut record_page = {
            let page = buffer_pool.fetch_page(record_id.page_id)?;
            if page.page_type() != PageType::Record {
                let _ = buffer_pool.unpin_page(record_id.page_id, false);
                return Err(Error::PageCorruption {
                    page_id: record_id.page_id,
                    reason: "expected Record page type tag",
                });
            }
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(record_id.page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(record_id.page_id, false)?;
            rp
        };

        let (is_active, overflow_descriptor) = match record_page.get_location(record_id.slot_id) {
            Some(RecordLocation::Inline(_)) => (true, None),
            Some(RecordLocation::Overflow(desc)) => (true, Some(desc)),
            None => (false, None),
        };

        if !is_active {
            return Ok((false, 0));
        }

        buffer_pool
            .check_fault(crate::engine::failure::StorageFailurePoint::DuringRecordSlotReclaim)?;

        // 1. Delete slot in RecordPage
        record_page.delete(record_id.slot_id);
        {
            let page_mut = buffer_pool.fetch_page_mut(record_id.page_id)?;
            record_page.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(record_id.page_id, true)?;

        // 2. If overflow-backed, reclaim all overflow pages into the freelist
        let mut overflow_count = 0u64;
        if let Some(descriptor) = overflow_descriptor {
            let mut curr = descriptor.head_page_id;
            let mut visited = HashSet::new();
            let total_pages = buffer_pool.page_count();
            let mut pages_to_free = Vec::new();

            while curr != NO_OVERFLOW_PAGE {
                if is_reserved_page(curr) || curr >= total_pages || !visited.insert(curr) {
                    break;
                }

                let page = buffer_pool.fetch_page(curr)?;
                if page.page_type() != PageType::Overflow {
                    let _ = buffer_pool.unpin_page(curr, false);
                    break;
                }

                let next = match OverflowPage::deserialize(page) {
                    Ok(op) => op.next_page_id(),
                    Err(_) => {
                        let _ = buffer_pool.unpin_page(curr, false);
                        break;
                    }
                };
                buffer_pool.unpin_page(curr, false)?;

                pages_to_free.push(curr);
                curr = next;
            }

            overflow_count = pages_to_free.len() as u64;
            for pid in pages_to_free {
                buffer_pool.free_page(pid)?;
            }
        }

        Ok((true, overflow_count))
    }

    /// Reclaims an entirely empty `PageType::Record` page into the persistent freelist if all
    /// slots are marked deleted.
    ///
    /// Returns `Ok(true)` if the page had zero active records and was successfully freed.
    pub fn try_free_empty_record_page(
        &mut self,
        buffer_pool: &mut BufferPool,
        page_id: PageId,
    ) -> Result<bool> {
        if is_reserved_page(page_id) || page_id >= buffer_pool.page_count() {
            return Ok(false);
        }

        let is_empty = {
            let page = buffer_pool.fetch_page(page_id)?;
            if page.page_type() != PageType::Record {
                let _ = buffer_pool.unpin_page(page_id, false);
                return Ok(false);
            }
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(page_id, false)?;
            rp.active_record_count() == 0
        };

        if is_empty {
            if self.last_page_id == page_id {
                self.last_page_id = INVALID_PAGE_ID;
            }
            buffer_pool.free_page(page_id)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Defragments a specific record page by consolidating all live record payloads and descriptors.
    pub fn compact_page(&mut self, buffer_pool: &mut BufferPool, page_id: PageId) -> Result<()> {
        if page_id == INVALID_PAGE_ID {
            return Ok(());
        }

        let mut record_page = {
            let page = buffer_pool.fetch_page(page_id)?;
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(page_id, false)?;
            rp
        };

        record_page.compact()?;

        {
            let page_mut = buffer_pool.fetch_page_mut(page_id)?;
            record_page.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(page_id, true)?;

        Ok(())
    }

    /// Validates the structural integrity of an overflow chain.
    ///
    /// Checks:
    /// 1. No reserved pages in the chain.
    /// 2. No out-of-bounds page IDs.
    /// 3. No cycles (no duplicate page IDs).
    /// 4. Every linked page is `PageType::Overflow`.
    /// 5. Every page has `bytes_used <= OVERFLOW_PAYLOAD_SIZE`.
    /// 6. Total bytes across all pages matches `expected_len`.
    /// 7. Intermediate pages are completely full (`bytes_used == OVERFLOW_PAYLOAD_SIZE`).
    pub fn validate_overflow_chain(
        &self,
        buffer_pool: &mut BufferPool,
        head_page_id: PageId,
        expected_len: u32,
    ) -> Result<HashSet<PageId>> {
        let mut curr = head_page_id;
        let mut visited = HashSet::new();
        let total_pages = buffer_pool.page_count();
        let mut total_bytes = 0usize;

        while curr != NO_OVERFLOW_PAGE {
            if is_reserved_page(curr) {
                return Err(Error::OverflowCorruption(format!(
                    "overflow chain contains reserved page {curr}"
                )));
            }
            if curr >= total_pages {
                return Err(Error::OverflowCorruption(format!(
                    "overflow chain contains out-of-bounds page {curr} (total pages: {total_pages})"
                )));
            }
            if !visited.insert(curr) {
                return Err(Error::OverflowCorruption(format!(
                    "cycle detected in overflow chain at page {curr}"
                )));
            }

            let page = buffer_pool.fetch_page(curr)?;
            if page.page_type() != PageType::Overflow {
                let _ = buffer_pool.unpin_page(curr, false);
                return Err(Error::PageCorruption {
                    page_id: curr,
                    reason: "expected Overflow page type tag",
                });
            }

            let op = match OverflowPage::deserialize(page) {
                Ok(op) => op,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr, false);
                    return Err(e);
                }
            };

            let bytes_used = op.bytes_used();
            let next = op.next_page_id();
            buffer_pool.unpin_page(curr, false)?;

            if bytes_used > OVERFLOW_PAYLOAD_SIZE {
                return Err(Error::OverflowCorruption(format!(
                    "overflow page {curr} bytes_used ({bytes_used}) exceeds capacity {OVERFLOW_PAYLOAD_SIZE}"
                )));
            }

            // Intermediate pages must be completely full
            if next != NO_OVERFLOW_PAGE && bytes_used != OVERFLOW_PAYLOAD_SIZE {
                return Err(Error::OverflowCorruption(format!(
                    "intermediate overflow page {curr} has partial occupancy {bytes_used} < {OVERFLOW_PAYLOAD_SIZE}"
                )));
            }

            total_bytes += bytes_used;
            curr = next;
        }

        if total_bytes != expected_len as usize {
            return Err(Error::OverflowCorruption(format!(
                "overflow chain total bytes ({total_bytes}) does not match expected length ({expected_len})"
            )));
        }

        Ok(visited)
    }

    /// Inserts a new MVCC version composed of a 44-byte `VersionMetadata` header followed by the user payload.
    pub fn insert_version(
        &mut self,
        buffer_pool: &mut BufferPool,
        metadata: &VersionMetadata,
        payload: &[u8],
    ) -> Result<RecordId> {
        let mut full_buf = vec![0u8; VERSION_HEADER_SIZE + payload.len()];
        metadata.serialize_into(&mut full_buf[..VERSION_HEADER_SIZE]);
        full_buf[VERSION_HEADER_SIZE..].copy_from_slice(payload);
        self.insert(buffer_pool, &full_buf)
    }

    /// Reads an individual physical MVCC version and splits it into `VersionMetadata` and user payload.
    pub fn read_version(
        &self,
        buffer_pool: &mut BufferPool,
        record_id: RecordId,
    ) -> Result<Option<(VersionMetadata, Vec<u8>)>> {
        let raw_bytes = match self.get(buffer_pool, record_id)? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let (metadata, payload) = VersionMetadata::deserialize(&raw_bytes)?;
        Ok(Some((metadata, payload.to_vec())))
    }

    /// Updates the persistent 44-byte MVCC metadata header of an existing record in place.
    pub fn update_version_metadata(
        &mut self,
        buffer_pool: &mut BufferPool,
        record_id: RecordId,
        metadata: &VersionMetadata,
    ) -> Result<()> {
        if record_id.page_id == INVALID_PAGE_ID {
            return Err(Error::InvalidRecordId(record_id));
        }

        let mut header_buf = [0u8; VERSION_HEADER_SIZE];
        metadata.serialize_into(&mut header_buf);

        let (is_overflow, head_page_id) = {
            let page = buffer_pool.fetch_page(record_id.page_id)?;
            if page.page_type() != PageType::Record {
                let _ = buffer_pool.unpin_page(record_id.page_id, false);
                return Err(Error::PageCorruption {
                    page_id: record_id.page_id,
                    reason: "expected Record page type tag",
                });
            }
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(record_id.page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(record_id.page_id, false)?;

            match rp.get_location(record_id.slot_id) {
                Some(RecordLocation::Inline(_)) => (false, NO_OVERFLOW_PAGE),
                Some(RecordLocation::Overflow(desc)) => (true, desc.head_page_id),
                None => return Err(Error::InvalidRecordId(record_id)),
            }
        };

        if !is_overflow {
            let page_mut = buffer_pool.fetch_page_mut(record_id.page_id)?;
            let mut rp = RecordPage::deserialize(page_mut)?;
            let inline_slice = rp
                .get_inline_payload_mut(record_id.slot_id)
                .ok_or(Error::InvalidRecordId(record_id))?;
            if inline_slice.len() < VERSION_HEADER_SIZE {
                let _ = buffer_pool.unpin_page(record_id.page_id, false);
                return Err(Error::InvalidVersionMetadata(format!(
                    "inline record size {} is smaller than header size {VERSION_HEADER_SIZE}",
                    inline_slice.len()
                )));
            }
            inline_slice[..VERSION_HEADER_SIZE].copy_from_slice(&header_buf);
            rp.serialize(page_mut)?;
            buffer_pool.unpin_page(record_id.page_id, true)?;
        } else {
            let page_mut = buffer_pool.fetch_page_mut(head_page_id)?;
            let mut op = OverflowPage::deserialize(page_mut)?;
            let overflow_payload = op.payload_mut();
            if overflow_payload.len() < VERSION_HEADER_SIZE {
                let _ = buffer_pool.unpin_page(head_page_id, false);
                return Err(Error::InvalidVersionMetadata(format!(
                    "overflow head payload size {} is smaller than header size {VERSION_HEADER_SIZE}",
                    overflow_payload.len()
                )));
            }
            overflow_payload[..VERSION_HEADER_SIZE].copy_from_slice(&header_buf);
            op.serialize(page_mut)?;
            buffer_pool.unpin_page(head_page_id, true)?;
        }

        buffer_pool.check_fault(
            crate::engine::failure::StorageFailurePoint::AfterVersionMetadataChanged,
        )?;

        Ok(())
    }

    /// Traverses the version chain starting at `head_rid` to find the first version visible to `snapshot`.
    ///
    /// Detects cycles and enforces `MAX_VERSION_CHAIN_DEPTH`.
    pub fn read_visible(
        &self,
        buffer_pool: &mut BufferPool,
        head_rid: RecordId,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> Result<Option<(RecordId, VersionMetadata, Vec<u8>)>> {
        let mut curr_rid = head_rid;
        let mut visited = HashSet::new();
        let mut depth = 0;

        while curr_rid.page_id != INVALID_PAGE_ID {
            if !visited.insert(curr_rid) {
                return Err(Error::VersionChainCycle(format!(
                    "cycle detected at {curr_rid:?} in version chain originating at head {head_rid:?}"
                )));
            }
            depth += 1;
            if depth > MAX_VERSION_CHAIN_DEPTH {
                return Err(Error::VersionChainTooDeep {
                    depth,
                    max_depth: MAX_VERSION_CHAIN_DEPTH,
                });
            }

            let (metadata, payload) = match self.read_version(buffer_pool, curr_rid)? {
                Some(v) => v,
                None => return Ok(None),
            };

            if MvccVisibilityPolicy::is_visible(
                metadata.xmin,
                metadata.xmax,
                metadata.created_at,
                metadata.deleted_at,
                reader_txn,
                snapshot,
            ) {
                return Ok(Some((curr_rid, metadata, payload)));
            }

            match metadata.prev_version {
                Some(prev) => {
                    if prev == curr_rid {
                        return Err(Error::VersionChainCycle(format!(
                            "self-referencing previous pointer at {curr_rid:?}"
                        )));
                    }
                    curr_rid = prev;
                }
                None => break,
            }
        }

        Ok(None)
    }

    /// Helper returning only the visible user payload for `head_rid`, if any.
    pub fn read_visible_payload(
        &self,
        buffer_pool: &mut BufferPool,
        head_rid: RecordId,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .read_visible(buffer_pool, head_rid, reader_txn, snapshot)?
            .map(|(_, _, p)| p))
    }

    /// Returns `true` if any version in the chain originating at `head_rid` is visible to `snapshot`.
    pub fn is_record_visible(
        &self,
        buffer_pool: &mut BufferPool,
        head_rid: RecordId,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> Result<bool> {
        Ok(self
            .read_visible(buffer_pool, head_rid, reader_txn, snapshot)?
            .is_some())
    }

    /// Checks whether a specific physical version is visible to `snapshot` and `reader_txn`.
    ///
    /// Unlike `is_record_visible`, this does NOT traverse `prev_version` links;
    /// it checks only whether the exact physical record identified by `rid` is visible.
    pub fn is_version_visible(
        &self,
        buffer_pool: &mut BufferPool,
        rid: RecordId,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> Result<bool> {
        let (metadata, _) = match self.read_version(buffer_pool, rid)? {
            Some(v) => v,
            None => return Ok(false),
        };
        Ok(MvccVisibilityPolicy::is_visible(
            metadata.xmin,
            metadata.xmax,
            metadata.created_at,
            metadata.deleted_at,
            reader_txn,
            snapshot,
        ))
    }

    /// Reads an individual physical MVCC version and returns its metadata and payload
    /// ONLY if that exact version is visible to `snapshot` and `reader_txn`.
    ///
    /// Unlike `read_visible`, this does NOT traverse `prev_version` links.
    pub fn read_exact_version_if_visible(
        &self,
        buffer_pool: &mut BufferPool,
        rid: RecordId,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> Result<Option<(VersionMetadata, Vec<u8>)>> {
        let (metadata, payload) = match self.read_version(buffer_pool, rid)? {
            Some(v) => v,
            None => return Ok(None),
        };
        if MvccVisibilityPolicy::is_visible(
            metadata.xmin,
            metadata.xmax,
            metadata.created_at,
            metadata.deleted_at,
            reader_txn,
            snapshot,
        ) {
            Ok(Some((metadata, payload)))
        } else {
            Ok(None)
        }
    }

    /// Validates the entire version chain starting from `head_rid`.
    ///
    /// Verifies:
    /// 1. Every linked record exists and can be deserialized.
    /// 2. No cycles.
    /// 3. Depth does not exceed `MAX_VERSION_CHAIN_DEPTH`.
    /// 4. Creation/deletion ordering invariants.
    /// 5. Intersite commit sequence monotonicity across the chain.
    ///
    /// Returns the length (depth) of the version chain.
    pub fn validate_version_chain(
        &self,
        buffer_pool: &mut BufferPool,
        head_rid: RecordId,
    ) -> Result<usize> {
        let mut curr_rid = head_rid;
        let mut visited = HashSet::new();
        let mut depth = 0;
        let mut next_created_at: Option<CommitSeq> = None;

        while curr_rid.page_id != INVALID_PAGE_ID {
            if !visited.insert(curr_rid) {
                return Err(Error::VersionChainCycle(format!(
                    "cycle detected at {curr_rid:?} in version chain originating at {head_rid:?}"
                )));
            }
            depth += 1;
            if depth > MAX_VERSION_CHAIN_DEPTH {
                return Err(Error::VersionChainTooDeep {
                    depth,
                    max_depth: MAX_VERSION_CHAIN_DEPTH,
                });
            }

            let (metadata, _) = self.read_version(buffer_pool, curr_rid)?.ok_or_else(|| {
                Error::InvalidVersionReference(format!("missing record at {curr_rid:?}"))
            })?;

            if let (Some(c), Some(d)) = (metadata.created_at, metadata.deleted_at) {
                if c > d {
                    return Err(Error::InvalidVersionMetadata(format!(
                        "created_at {c} > deleted_at {d} at {curr_rid:?}"
                    )));
                }
            }

            if let (Some(newer_created), Some(older_created)) =
                (next_created_at, metadata.created_at)
            {
                if older_created > newer_created {
                    return Err(Error::InvalidCommitSequence(format!(
                        "older version commit seq {older_created} > newer version commit seq {newer_created}"
                    )));
                }
            }
            next_created_at = metadata.created_at;

            match metadata.prev_version {
                Some(prev) => {
                    if prev == curr_rid {
                        return Err(Error::VersionChainCycle(format!(
                            "self-referencing previous pointer at {curr_rid:?}"
                        )));
                    }
                    curr_rid = prev;
                }
                None => break,
            }
        }

        Ok(depth)
    }
}
