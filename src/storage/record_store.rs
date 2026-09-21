use crate::error::{Error, Result};
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{PageId, PageType};
use crate::storage::record_page::{RecordPage, MAX_RECORD_SIZE};

/// Persistent record storage manager coordinating variable-length record pages.
///
/// `RecordStore` allocates, reads, deletes, and compacts slotted records across
/// physical database pages via the `BufferPool`.
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

    /// Inserts a variable-length record payload into a slotted record page.
    ///
    /// If the current active record page cannot fit the record, a new `PageType::Record`
    /// page is allocated automatically through the `BufferPool`.
    ///
    /// # Errors
    /// * `Error::ValueTooLarge` if `record.len() > MAX_RECORD_SIZE` (4078 bytes).
    /// * `Error::BufferPoolFull` or `Error::Io` on storage or capacity limits.
    pub fn insert(&mut self, buffer_pool: &mut BufferPool, record: &[u8]) -> Result<RecordId> {
        let record_len = record.len();
        if record_len > MAX_RECORD_SIZE {
            return Err(Error::ValueTooLarge {
                size: record_len,
                limit: MAX_RECORD_SIZE,
            });
        }

        // 1. Determine target record page
        let (target_page_id, mut record_page) = if self.last_page_id != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(self.last_page_id)?;
            let rp = match RecordPage::deserialize(page) {
                Ok(rp) => rp,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(self.last_page_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(self.last_page_id, false)?;

            if rp.can_fit(record_len) {
                (self.last_page_id, rp)
            } else {
                let new_page_id = buffer_pool.allocate_page(PageType::Record)?;
                buffer_pool.unpin_page(new_page_id, true)?;
                self.last_page_id = new_page_id;
                (new_page_id, RecordPage::new())
            }
        } else {
            let new_page_id = buffer_pool.allocate_page(PageType::Record)?;
            buffer_pool.unpin_page(new_page_id, true)?;
            self.last_page_id = new_page_id;
            (new_page_id, RecordPage::new())
        };

        // 2. Insert record into target slotted page
        let slot_id = record_page.insert(record)?;

        // 3. Serialize updated page back to BufferPool
        {
            let page_mut = buffer_pool.fetch_page_mut(target_page_id)?;
            record_page.serialize(page_mut)?;
        }
        buffer_pool.unpin_page(target_page_id, true)?;

        Ok(RecordId::new(target_page_id, slot_id))
    }

    /// Retrieves an owned copy of the record bytes associated with `record_id`.
    ///
    /// Returns `Ok(Some(Vec<u8>))` if found, or `Ok(None)` if the slot is deleted or out of bounds.
    ///
    /// # Pinning Discipline
    /// The page frame is promptly unpinned before returning, holding zero Buffer Pool pins on exit.
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

        let record_data = record_page.get(record_id.slot_id).map(|b| b.to_vec());
        buffer_pool.unpin_page(record_id.page_id, false)?;

        Ok(record_data)
    }

    /// Logically deletes the record associated with `record_id`.
    ///
    /// Marks the slot as unused. Returns `true` if the record existed and was deleted,
    /// or `false` if already deleted or out of bounds.
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

        let deleted = record_page.delete(record_id.slot_id);
        if deleted {
            let page_mut = buffer_pool.fetch_page_mut(record_id.page_id)?;
            record_page.serialize(page_mut)?;
            buffer_pool.unpin_page(record_id.page_id, true)?;
        }

        Ok(deleted)
    }

    /// Defragments a specific record page by consolidating all live record payloads.
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
}
