use crate::storage::page::{Key, PageId, PAGE_PAYLOAD_SIZE};

/// Sentinel indicating an invalid or unassigned page identifier.
pub const INVALID_PAGE_ID: PageId = 0;

/// Identifier referencing a physical tuple/record in the storage engine.
///
/// Encodes the target `PageId` and slot index within that page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RecordId {
    /// Target database page ID holding the record.
    pub page_id: PageId,
    /// Slot offset within the slotted record page.
    pub slot_id: u32,
}

impl RecordId {
    /// Creates a new `RecordId`.
    pub const fn new(page_id: PageId, slot_id: u32) -> Self {
        Self { page_id, slot_id }
    }

    /// Serializes `RecordId` into an 8-byte little-endian array.
    pub fn to_bytes(self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&self.page_id.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.slot_id.to_le_bytes());
        bytes
    }

    /// Deserializes `RecordId` from an 8-byte little-endian array.
    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        let page_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let slot_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        Self { page_id, slot_id }
    }
}

// ---------------------------------------------------------------------------
// Capacity Constants
// ---------------------------------------------------------------------------

/// Size in bytes of a serialized Key (u64).
pub const KEY_SIZE: usize = std::mem::size_of::<Key>(); // 8 bytes

/// Size in bytes of a serialized RecordId (page_id: u32 + slot_id: u32).
pub const RECORD_ID_SIZE: usize = std::mem::size_of::<RecordId>(); // 8 bytes

/// Size in bytes of a serialized PageId (u32).
pub const PAGE_ID_SIZE: usize = std::mem::size_of::<PageId>(); // 4 bytes

/// Leaf node header size in payload: `key_count` (2 bytes) + `next_leaf` (4 bytes).
pub const LEAF_HEADER_SIZE: usize = 6;

/// Width in bytes of a single leaf entry: `Key` (8 bytes) + `RecordId` (8 bytes).
pub const LEAF_ENTRY_SIZE: usize = KEY_SIZE + RECORD_ID_SIZE; // 16 bytes

/// Maximum number of key-record entries that fit in a Leaf page payload.
///
/// Derived as: `(4088 - 6) / 16 = 255 entries` (4080 bytes used of 4088 available).
pub const LEAF_MAX_ENTRIES: usize = (PAGE_PAYLOAD_SIZE - LEAF_HEADER_SIZE) / LEAF_ENTRY_SIZE;

/// Internal node header size in payload: `key_count` (2 bytes) + `first_child` (4 bytes).
pub const INTERNAL_HEADER_SIZE: usize = 6;

/// Width in bytes of an internal separator entry: `Key` (8 bytes) + `child_page_id` (4 bytes).
pub const INTERNAL_ENTRY_SIZE: usize = KEY_SIZE + PAGE_ID_SIZE; // 12 bytes

/// Maximum number of separator keys that fit in an Internal page payload.
///
/// Derived as: `(4088 - 6) / 12 = 340 keys` (4080 bytes used of 4088 available).
/// An internal node with 340 keys routes to 341 child pages.
pub const INTERNAL_MAX_KEYS: usize =
    (PAGE_PAYLOAD_SIZE - INTERNAL_HEADER_SIZE) / INTERNAL_ENTRY_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_id_roundtrip() {
        let rid = RecordId::new(42, 7);
        let bytes = rid.to_bytes();
        let restored = RecordId::from_bytes(bytes);
        assert_eq!(restored, rid);
    }

    #[test]
    fn test_capacity_derivations() {
        assert_eq!(LEAF_MAX_ENTRIES, 255);
        const { assert!(LEAF_HEADER_SIZE + (LEAF_MAX_ENTRIES * LEAF_ENTRY_SIZE) <= PAGE_PAYLOAD_SIZE) };

        assert_eq!(INTERNAL_MAX_KEYS, 340);
        const {
            assert!(
                INTERNAL_HEADER_SIZE + (INTERNAL_MAX_KEYS * INTERNAL_ENTRY_SIZE)
                    <= PAGE_PAYLOAD_SIZE
            )
        };
    }
}
