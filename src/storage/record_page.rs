use crate::error::{Error, Result};
use crate::storage::page::{Page, PageType, PAGE_PAYLOAD_SIZE};

/// Size in bytes of the slotted record page header within the payload.
///
/// Layout: `slot_count` (2B) + `free_space_start` (2B) + `free_space_end` (2B) = 6 bytes.
pub const RECORD_PAGE_HEADER_SIZE: usize = 6;

/// Size in bytes of a single slot directory entry.
///
/// Layout: `offset` (2B) + `length` (2B) = 4 bytes.
pub const SLOT_SIZE: usize = 4;

/// Maximum payload size in bytes for a single record fitting entirely on one page.
///
/// Derived as: `PAGE_PAYLOAD_SIZE (4080) - RECORD_PAGE_HEADER_SIZE (6) - SLOT_SIZE (4) = 4070 bytes`.
pub const MAX_RECORD_SIZE: usize = PAGE_PAYLOAD_SIZE - RECORD_PAGE_HEADER_SIZE - SLOT_SIZE;

/// Slot directory entry pointing to a variable-length record payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slot {
    /// Byte offset within the page payload where the record bytes begin.
    ///
    /// An offset of `0` indicates a deleted or unused slot.
    pub offset: u16,
    /// Length of the record payload in bytes.
    pub length: u16,
}

impl Slot {
    /// Creates a new active slot descriptor.
    pub const fn new(offset: u16, length: u16) -> Self {
        Self { offset, length }
    }

    /// Creates a deleted/unused slot descriptor.
    pub const fn deleted() -> Self {
        Self {
            offset: 0,
            length: 0,
        }
    }

    /// Returns `true` if this slot is deleted or unused.
    pub const fn is_deleted(&self) -> bool {
        self.offset == 0
    }

    /// Returns `true` if this slot points to an active record.
    pub const fn is_active(&self) -> bool {
        self.offset != 0
    }
}

/// In-memory representation of a Slotted Record Page (`PageType::Record`).
///
/// # Slotted Page Layout (Payload bytes 8..4096)
/// ```text
/// +---------------------------------------------------------------+
/// | Page Header (8 bytes): [page_id: u32 | type=4 | flags | ...]  |
/// +---------------------------------------------------------------+
/// | Record Page Header (6 bytes):                                 |
/// |   slot_count: u16 (2B) | free_start: u16 (2B) | free_end: u16 |
/// +---------------------------------------------------------------+
/// | Slot Directory (grows upward):                                |
/// |   Slot 0: [offset: u16 | length: u16]                         |
/// |   Slot 1: [offset: u16 | length: u16]                         |
/// |   ...                                                         |
/// +---------------------------------------------------------------+
/// | Free Space Region (contiguous gap between free_start & end)   |
/// +---------------------------------------------------------------+
/// | Record Payloads (grows downward from 4088):                   |
/// |   Record N payload                                            |
/// |   ...                                                         |
/// |   Record 0 payload                                            |
/// +---------------------------------------------------------------+
/// ```
#[derive(Debug, Clone)]
pub struct RecordPage {
    /// Slot directory entries (both active and deleted).
    pub slots: Vec<Slot>,
    /// Contiguous payload buffer holding all raw page payload bytes.
    payload_data: [u8; PAGE_PAYLOAD_SIZE],
    /// Free space pointer at the end of the slot directory (grows upward).
    free_space_start: u16,
    /// Free space pointer at the beginning of record payloads (grows downward).
    free_space_end: u16,
}

impl PartialEq for RecordPage {
    fn eq(&self, other: &Self) -> bool {
        if self.slots != other.slots
            || self.free_space_start != other.free_space_start
            || self.free_space_end != other.free_space_end
        {
            return false;
        }

        for (i, slot) in self.slots.iter().enumerate() {
            if slot.is_active() && self.get(i as u32) != other.get(i as u32) {
                return false;
            }
        }
        true
    }
}

impl Eq for RecordPage {}

impl Default for RecordPage {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordPage {
    /// Creates a new empty slotted record page.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            payload_data: [0u8; PAGE_PAYLOAD_SIZE],
            free_space_start: RECORD_PAGE_HEADER_SIZE as u16,
            free_space_end: PAGE_PAYLOAD_SIZE as u16,
        }
    }

    /// Returns the total number of slots in the directory (including deleted slots).
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Returns the number of currently active (non-deleted) records.
    pub fn active_record_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_active()).count()
    }

    /// Returns the size in bytes of contiguous unallocated free space.
    pub fn contiguous_free_space(&self) -> usize {
        (self.free_space_end.saturating_sub(self.free_space_start)) as usize
    }

    /// Returns the total available space including fragmented holes from deleted records.
    pub fn total_free_space(&self) -> usize {
        let active_payload_sum: usize = self
            .slots
            .iter()
            .filter(|s| s.is_active())
            .map(|s| s.length as usize)
            .sum();
        let slot_dir_size = self.slots.len() * SLOT_SIZE;
        let used_space = RECORD_PAGE_HEADER_SIZE + slot_dir_size + active_payload_sum;
        PAGE_PAYLOAD_SIZE.saturating_sub(used_space)
    }

    /// Checks whether a record of length `record_len` can fit in this page (possibly after compaction).
    pub fn can_fit(&self, record_len: usize) -> bool {
        if record_len > MAX_RECORD_SIZE {
            return false;
        }

        let has_reusable_slot = self.slots.iter().any(|s| s.is_deleted());
        let required_space = if has_reusable_slot {
            record_len
        } else {
            record_len + SLOT_SIZE
        };

        self.total_free_space() >= required_space
    }

    /// Inserts a variable-length record payload into the page, automatically compacting if fragmented.
    ///
    /// # Returns
    /// * `Ok(slot_id)`: Index of the allocated slot.
    /// * `Err(Error::ValueTooLarge)`: If the record cannot fit on the page even after compaction.
    pub fn insert(&mut self, record: &[u8]) -> Result<u32> {
        let record_len = record.len();
        if record_len > MAX_RECORD_SIZE {
            return Err(Error::ValueTooLarge {
                size: record_len,
                limit: MAX_RECORD_SIZE,
            });
        }

        // Check if there is an existing deleted slot we can reuse
        let reusable_slot_idx = self.slots.iter().position(|s| s.is_deleted());
        let required_contiguous = if reusable_slot_idx.is_some() {
            record_len
        } else {
            record_len + SLOT_SIZE
        };

        // If not enough contiguous space, but total free space is sufficient, compact first!
        if self.contiguous_free_space() < required_contiguous {
            if self.total_free_space() < required_contiguous {
                return Err(Error::ValueTooLarge {
                    size: record_len,
                    limit: self.total_free_space(),
                });
            }
            self.compact()?;
        }

        // Allocate payload bytes at the bottom of free space
        let new_offset = self.free_space_end - record_len as u16;
        let start_idx = new_offset as usize;
        let end_idx = start_idx + record_len;

        self.payload_data[start_idx..end_idx].copy_from_slice(record);
        self.free_space_end = new_offset;

        let slot_id = match reusable_slot_idx {
            Some(idx) => {
                self.slots[idx] = Slot::new(new_offset, record_len as u16);
                idx as u32
            }
            None => {
                let idx = self.slots.len();
                self.slots.push(Slot::new(new_offset, record_len as u16));
                self.free_space_start += SLOT_SIZE as u16;
                idx as u32
            }
        };

        Ok(slot_id)
    }

    /// Retrieves a reference to the record payload stored at `slot_id`.
    ///
    /// Returns `None` if `slot_id` is out of bounds or points to a deleted record.
    pub fn get(&self, slot_id: u32) -> Option<&[u8]> {
        let slot = self.slots.get(slot_id as usize)?;
        if slot.is_deleted() {
            return None;
        }

        let start = slot.offset as usize;
        let end = start + slot.length as usize;

        if end > PAGE_PAYLOAD_SIZE || start < self.free_space_start as usize {
            return None;
        }

        Some(&self.payload_data[start..end])
    }

    /// Logically deletes the record at `slot_id`.
    ///
    /// Sets the slot descriptor to deleted (`offset: 0, length: 0`).
    /// Returns `true` if the record existed and was deleted, or `false` if already deleted or out of bounds.
    pub fn delete(&mut self, slot_id: u32) -> bool {
        if let Some(slot) = self.slots.get_mut(slot_id as usize) {
            if slot.is_active() {
                *slot = Slot::deleted();
                return true;
            }
        }
        false
    }

    /// Defragments the payload area of the page by consolidating all active record payloads.
    ///
    /// Guarantees that:
    /// 1. All active payloads are packed contiguously at the high end of the page.
    /// 2. All slot IDs and logical `RecordId` references remain 100% stable.
    /// 3. `free_space_end` is updated to reflect the contiguous reclaimed free space.
    pub fn compact(&mut self) -> Result<()> {
        let mut new_payload = [0u8; PAGE_PAYLOAD_SIZE];
        let mut current_end = PAGE_PAYLOAD_SIZE as u16;

        // Iterate over all active slots and copy their payloads to the high end
        for slot in &mut self.slots {
            if slot.is_active() {
                let old_start = slot.offset as usize;
                let old_end = old_start + slot.length as usize;

                if old_end > PAGE_PAYLOAD_SIZE {
                    return Err(Error::PageCorruption {
                        page_id: 0,
                        reason: "slot offset/length exceeds payload bounds during compaction",
                    });
                }

                let len = slot.length;
                let new_start = current_end - len;

                new_payload[new_start as usize..current_end as usize]
                    .copy_from_slice(&self.payload_data[old_start..old_end]);

                slot.offset = new_start;
                current_end = new_start;
            }
        }

        self.payload_data = new_payload;
        self.free_space_end = current_end;
        self.free_space_start = (RECORD_PAGE_HEADER_SIZE + self.slots.len() * SLOT_SIZE) as u16;

        self.validate()?;
        Ok(())
    }

    /// Validates internal consistency and invariants of the slotted record page.
    pub fn validate(&self) -> Result<()> {
        if self.free_space_start > self.free_space_end {
            return Err(Error::PageCorruption {
                page_id: 0,
                reason: "free space start overlaps with free space end",
            });
        }

        let expected_free_start = (RECORD_PAGE_HEADER_SIZE + self.slots.len() * SLOT_SIZE) as u16;
        if self.free_space_start != expected_free_start {
            return Err(Error::PageCorruption {
                page_id: 0,
                reason: "free space start does not match slot directory size",
            });
        }

        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.is_active() {
                let start = slot.offset as usize;
                let end = start + slot.length as usize;

                if start < self.free_space_end as usize || end > PAGE_PAYLOAD_SIZE {
                    return Err(Error::TreeCorruption(format!(
                        "slot {idx} out of bounds: offset {start}, length {}, payload size {PAGE_PAYLOAD_SIZE}",
                        slot.length
                    )));
                }
            }
        }

        Ok(())
    }

    /// Serializes the slotted record page into the provided `Page` buffer.
    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        self.validate()?;

        page.set_page_type(PageType::Record);
        let payload = page.payload_mut();

        // 1. Write Record Page Header (6 bytes)
        let slot_count = self.slots.len() as u16;
        payload[0..2].copy_from_slice(&slot_count.to_le_bytes());
        payload[2..4].copy_from_slice(&self.free_space_start.to_le_bytes());
        payload[4..6].copy_from_slice(&self.free_space_end.to_le_bytes());

        // 2. Write Slot Directory
        let mut offset = RECORD_PAGE_HEADER_SIZE;
        for slot in &self.slots {
            payload[offset..offset + 2].copy_from_slice(&slot.offset.to_le_bytes());
            payload[offset + 2..offset + 4].copy_from_slice(&slot.length.to_le_bytes());
            offset += SLOT_SIZE;
        }

        // 3. Write Free Space (zero-filled)
        payload[self.free_space_start as usize..self.free_space_end as usize].fill(0);

        // 4. Write Record Payloads
        let payload_start = self.free_space_end as usize;
        payload[payload_start..PAGE_PAYLOAD_SIZE]
            .copy_from_slice(&self.payload_data[payload_start..PAGE_PAYLOAD_SIZE]);

        Ok(())
    }

    /// Deserializes a `RecordPage` from a `Page`, validating page type and structural layout.
    pub fn deserialize(page: &Page) -> Result<Self> {
        if page.page_type() != PageType::Record {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "expected Record page type tag",
            });
        }

        let payload = page.payload();

        let slot_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let free_space_start = u16::from_le_bytes([payload[2], payload[3]]);
        let free_space_end = u16::from_le_bytes([payload[4], payload[5]]);

        // Freshly allocated uninitialized record page
        if slot_count == 0 && free_space_start == 0 && free_space_end == 0 {
            return Ok(Self::new());
        }

        let max_slots = (PAGE_PAYLOAD_SIZE - RECORD_PAGE_HEADER_SIZE) / SLOT_SIZE;
        if slot_count > max_slots {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "slot count exceeds maximum possible page capacity",
            });
        }

        let expected_free_start = (RECORD_PAGE_HEADER_SIZE + slot_count * SLOT_SIZE) as u16;
        if free_space_start != expected_free_start {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "corrupted free space start offset",
            });
        }

        if free_space_start > free_space_end || free_space_end as usize > PAGE_PAYLOAD_SIZE {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "invalid free space boundary offsets",
            });
        }

        let mut slots = Vec::with_capacity(slot_count);
        let mut offset = RECORD_PAGE_HEADER_SIZE;

        for _ in 0..slot_count {
            let slot_offset = u16::from_le_bytes([payload[offset], payload[offset + 1]]);
            let slot_length = u16::from_le_bytes([payload[offset + 2], payload[offset + 3]]);

            if slot_offset != 0 {
                let start = slot_offset as usize;
                let end = start + slot_length as usize;
                if start < free_space_end as usize || end > PAGE_PAYLOAD_SIZE {
                    return Err(Error::PageCorruption {
                        page_id: page.id(),
                        reason: "slot references payload out of valid boundaries",
                    });
                }
            }

            slots.push(Slot::new(slot_offset, slot_length));
            offset += SLOT_SIZE;
        }

        let mut payload_data = [0u8; PAGE_PAYLOAD_SIZE];
        payload_data.copy_from_slice(payload);

        let record_page = Self {
            slots,
            payload_data,
            free_space_start,
            free_space_end,
        };

        record_page.validate()?;
        Ok(record_page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_page_empty_roundtrip() {
        let rp = RecordPage::new();
        assert_eq!(rp.slot_count(), 0);
        assert_eq!(rp.active_record_count(), 0);
        assert_eq!(rp.contiguous_free_space(), 4074);
        assert_eq!(rp.total_free_space(), 4074);

        let mut page = Page::new(10, PageType::Record);
        rp.serialize(&mut page).expect("serialize");

        let restored = RecordPage::deserialize(&page).expect("deserialize");
        assert_eq!(restored, rp);
    }

    #[test]
    fn test_record_page_insert_get_delete() {
        let mut rp = RecordPage::new();

        let s0 = rp.insert(b"hello world").expect("insert 1");
        assert_eq!(s0, 0);
        let s1 = rp.insert(b"foo bar baz").expect("insert 2");
        assert_eq!(s1, 1);

        assert_eq!(rp.get(0), Some(&b"hello world"[..]));
        assert_eq!(rp.get(1), Some(&b"foo bar baz"[..]));
        assert_eq!(rp.get(2), None);

        assert!(rp.delete(0));
        assert_eq!(rp.get(0), None);
        assert_eq!(rp.get(1), Some(&b"foo bar baz"[..]));

        // Slot reuse
        let s2 = rp.insert(b"reused slot!").expect("insert 3");
        assert_eq!(s2, 0); // Reused slot 0!
        assert_eq!(rp.get(0), Some(&b"reused slot!"[..]));
        assert_eq!(rp.get(1), Some(&b"foo bar baz"[..]));
    }

    #[test]
    fn test_record_page_compaction() {
        let mut rp = RecordPage::new();

        let a = rp.insert(b"AAAAA").expect("insert A");
        let b = rp.insert(b"BBBBB").expect("insert B");
        let c = rp.insert(b"CCCCC").expect("insert C");

        assert!(rp.delete(b));

        let free_before = rp.contiguous_free_space();
        rp.compact().expect("compact");
        let free_after = rp.contiguous_free_space();

        assert!(free_after > free_before);
        assert_eq!(rp.get(a), Some(&b"AAAAA"[..]));
        assert_eq!(rp.get(b), None);
        assert_eq!(rp.get(c), Some(&b"CCCCC"[..]));
    }

    #[test]
    fn test_max_record_boundary() {
        let mut rp = RecordPage::new();
        let max_data = vec![42u8; MAX_RECORD_SIZE];
        let slot = rp.insert(&max_data).expect("insert max");
        assert_eq!(slot, 0);
        assert_eq!(rp.get(0), Some(&max_data[..]));

        // Second record cannot fit
        let err = rp.insert(b"a").unwrap_err();
        assert!(matches!(err, Error::ValueTooLarge { .. }));
    }
}
