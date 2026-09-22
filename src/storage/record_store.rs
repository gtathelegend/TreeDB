use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::metadata::is_reserved_page;
use crate::storage::overflow_page::{OverflowPage, NO_OVERFLOW_PAGE, OVERFLOW_PAYLOAD_SIZE};
use crate::storage::page::{PageId, PageType};
use crate::storage::record_page::{
    RecordLocation, RecordPage, MAX_RECORD_SIZE, OVERFLOW_DESCRIPTOR_SIZE,
};

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

            return Ok(RecordId::new(target_page_id, slot_id));
        }

        // Path B: Multi-Page Overflow Storage (> MAX_RECORD_SIZE)
        let total_len = record_len;
        let overflow_pages_needed = total_len.div_ceil(OVERFLOW_PAYLOAD_SIZE);

        // 1. Allocate all required overflow pages from the BufferPool
        let mut allocated_pages = Vec::with_capacity(overflow_pages_needed);
        for _ in 0..overflow_pages_needed {
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
        if record_id.page_id == INVALID_PAGE_ID {
            return Ok(false);
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
            return Ok(false);
        }

        // 1. Delete slot in RecordPage
        record_page.delete(record_id.slot_id);
        {
            let page_mut = buffer_pool.fetch_page_mut(record_id.page_id)?;
            record_page.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(record_id.page_id, true)?;

        // 2. If overflow-backed, reclaim all overflow pages into the freelist
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

            for pid in pages_to_free {
                buffer_pool.free_page(pid)?;
            }
        }

        Ok(true)
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
}
