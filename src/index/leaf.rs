use crate::error::{Error, Result};
use crate::index::node::RecordId;
use crate::index::node::{
    INVALID_PAGE_ID, LEAF_ENTRY_SIZE, LEAF_HEADER_SIZE, LEAF_MAX_ENTRIES, RECORD_ID_SIZE,
};
use crate::storage::page::{Key, Page, PageId, PageType};

/// In-memory representation of a B+ Tree Leaf Node.
///
/// Leaf nodes store sorted `(Key, RecordId)` pairs and link to sibling leaves
/// via `next_leaf` for ordered sequential traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafNode {
    /// Pointer to the next sibling leaf page, or `INVALID_PAGE_ID` (0) if rightmost.
    pub next_leaf: PageId,
    /// Sorted key-to-record mappings.
    pub entries: Vec<(Key, RecordId)>,
}

impl Default for LeafNode {
    fn default() -> Self {
        Self::new()
    }
}

impl LeafNode {
    /// Creates an empty leaf node with no sibling link.
    pub fn new() -> Self {
        Self {
            next_leaf: INVALID_PAGE_ID,
            entries: Vec::new(),
        }
    }

    /// Creates an empty leaf node with an explicit `next_leaf` sibling link.
    pub fn with_next_leaf(next_leaf: PageId) -> Self {
        Self {
            next_leaf,
            entries: Vec::new(),
        }
    }

    /// Searches for a key within the leaf node using binary search.
    ///
    /// Returns `Some(RecordId)` if found, or `None` if the key does not exist.
    pub fn get(&self, key: Key) -> Option<RecordId> {
        match self.entries.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(idx) => Some(self.entries[idx].1),
            Err(_) => None,
        }
    }

    /// Returns the number of entries currently stored in the leaf.
    pub fn key_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the leaf contains zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns true if the leaf has reached its maximum entry capacity.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= LEAF_MAX_ENTRIES
    }

    /// Inserts a `(Key, RecordId)` entry into a non-full leaf node in sorted order.
    ///
    /// # Errors
    /// * `Error::DuplicateKey` if `key` already exists.
    /// * `Error::ValueTooLarge` if the leaf is already full.
    pub fn insert(&mut self, key: Key, record_id: RecordId) -> Result<()> {
        match self.entries.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(_) => Err(Error::DuplicateKey(key)),
            Err(idx) => {
                if self.is_full() {
                    return Err(Error::ValueTooLarge {
                        size: self.entries.len() + 1,
                        limit: LEAF_MAX_ENTRIES,
                    });
                }
                self.entries.insert(idx, (key, record_id));
                Ok(())
            }
        }
    }

    /// Inserts a `(Key, RecordId)` entry into a full leaf node and splits it into two balanced leaves.
    ///
    /// Returns the separator key promoted to the parent (which is the first key of the new right leaf)
    /// and the newly created `LeafNode` (right half).
    ///
    /// Updates `self.next_leaf` to point to `right_page_id`, and `right_leaf.next_leaf` receives
    /// the old `self.next_leaf`.
    pub fn split_insert(
        &mut self,
        key: Key,
        record_id: RecordId,
        right_page_id: PageId,
    ) -> Result<(Key, LeafNode)> {
        match self.entries.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(_) => Err(Error::DuplicateKey(key)),
            Err(idx) => {
                self.entries.insert(idx, (key, record_id));

                let mid = self.entries.len() / 2;
                let right_entries = self.entries.split_off(mid);

                let right_leaf = LeafNode {
                    next_leaf: self.next_leaf,
                    entries: right_entries,
                };

                self.next_leaf = right_page_id;
                let sep_key = right_leaf.entries[0].0;

                Ok((sep_key, right_leaf))
            }
        }
    }

    /// Serializes the leaf node into the provided `Page` buffer.
    ///
    /// # Layout (Payload bytes 8..4096)
    /// * Offset 0..2: `key_count` (`u16`, LE)
    /// * Offset 2..6: `next_leaf` (`u32`, LE)
    /// * Offset 6..: Array of entries `[Key (8B) | RecordId (8B)]`
    ///
    /// # Invariants Validated
    /// * `entries.len() <= LEAF_MAX_ENTRIES` (255)
    /// * Keys must be strictly ascending: `keys[i] < keys[i+1]`
    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        if self.entries.len() > LEAF_MAX_ENTRIES {
            return Err(Error::ValueTooLarge {
                size: self.entries.len(),
                limit: LEAF_MAX_ENTRIES,
            });
        }

        // Validate strictly ascending key order
        for i in 1..self.entries.len() {
            if self.entries[i].0 <= self.entries[i - 1].0 {
                return Err(Error::TreeCorruption(format!(
                    "leaf node entries not strictly increasing: key {} at index {} <= key {} at index {}",
                    self.entries[i].0,
                    i,
                    self.entries[i - 1].0,
                    i - 1
                )));
            }
        }

        page.set_page_type(PageType::Leaf);
        let payload = page.payload_mut();

        // 1. Write header
        let key_count = self.entries.len() as u16;
        payload[0..2].copy_from_slice(&key_count.to_le_bytes());
        payload[2..6].copy_from_slice(&self.next_leaf.to_le_bytes());

        // 2. Write entries
        let mut offset = LEAF_HEADER_SIZE;
        for &(key, rid) in &self.entries {
            payload[offset..offset + 8].copy_from_slice(&key.to_le_bytes());
            payload[offset + 8..offset + 16].copy_from_slice(&rid.to_bytes());
            offset += LEAF_ENTRY_SIZE;
        }

        // Zero-fill remaining unused payload
        payload[offset..].fill(0);

        Ok(())
    }

    /// Deserializes a `LeafNode` from a `Page`, performing structural validation.
    ///
    /// # Errors
    /// * `Error::PageCorruption` if page type is not `Leaf`, entry count exceeds capacity,
    ///   or keys are not strictly increasing.
    pub fn deserialize(page: &Page) -> Result<Self> {
        if page.page_type() != PageType::Leaf {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "expected Leaf page type tag",
            });
        }

        let payload = page.payload();
        let key_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        if key_count > LEAF_MAX_ENTRIES {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "leaf entry count exceeds maximum capacity",
            });
        }

        let next_leaf = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);

        let mut entries: Vec<(Key, RecordId)> = Vec::with_capacity(key_count);
        let mut offset = LEAF_HEADER_SIZE;

        for i in 0..key_count {
            let key = u64::from_le_bytes([
                payload[offset],
                payload[offset + 1],
                payload[offset + 2],
                payload[offset + 3],
                payload[offset + 4],
                payload[offset + 5],
                payload[offset + 6],
                payload[offset + 7],
            ]);

            let mut rid_bytes = [0u8; RECORD_ID_SIZE];
            rid_bytes.copy_from_slice(&payload[offset + 8..offset + 16]);
            let rid = RecordId::from_bytes(rid_bytes);

            if i > 0 && key <= entries[i - 1].0 {
                return Err(Error::PageCorruption {
                    page_id: page.id(),
                    reason: "leaf keys are not strictly increasing",
                });
            }

            entries.push((key, rid));
            offset += LEAF_ENTRY_SIZE;
        }

        Ok(Self { next_leaf, entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf_roundtrip_and_get() {
        let mut leaf = LeafNode::new();
        leaf.next_leaf = 10;
        leaf.entries.push((100, RecordId::new(5, 1)));
        leaf.entries.push((200, RecordId::new(5, 2)));
        leaf.entries.push((300, RecordId::new(6, 1)));

        assert_eq!(leaf.get(100), Some(RecordId::new(5, 1)));
        assert_eq!(leaf.get(200), Some(RecordId::new(5, 2)));
        assert_eq!(leaf.get(300), Some(RecordId::new(6, 1)));
        assert_eq!(leaf.get(150), None);
        assert_eq!(leaf.get(50), None);
        assert_eq!(leaf.get(400), None);

        let mut page = Page::new(42, PageType::Leaf);
        leaf.serialize(&mut page).expect("serialize");

        let restored = LeafNode::deserialize(&page).expect("deserialize");
        assert_eq!(restored, leaf);
    }

    #[test]
    fn test_leaf_unsorted_keys_rejected() {
        let mut leaf = LeafNode::new();
        leaf.entries.push((200, RecordId::new(1, 1)));
        leaf.entries.push((100, RecordId::new(1, 2))); // Unsorted!

        let mut page = Page::new(42, PageType::Leaf);
        let err = leaf.serialize(&mut page).unwrap_err();
        assert!(matches!(err, Error::TreeCorruption(_)));
    }

    #[test]
    fn test_leaf_duplicate_keys_rejected() {
        let mut leaf = LeafNode::new();
        leaf.entries.push((100, RecordId::new(1, 1)));
        leaf.entries.push((100, RecordId::new(1, 2))); // Duplicate!

        let mut page = Page::new(42, PageType::Leaf);
        let err = leaf.serialize(&mut page).unwrap_err();
        assert!(matches!(err, Error::TreeCorruption(_)));
    }

    #[test]
    fn test_leaf_max_capacity() {
        let mut leaf = LeafNode::new();
        for i in 0..LEAF_MAX_ENTRIES {
            leaf.entries.push((i as u64, RecordId::new(2, i as u32)));
        }
        assert!(leaf.is_full());

        let mut page = Page::new(42, PageType::Leaf);
        leaf.serialize(&mut page).expect("serialize max");
        let restored = LeafNode::deserialize(&page).expect("deserialize max");
        assert_eq!(restored.key_count(), LEAF_MAX_ENTRIES);
        assert_eq!(restored, leaf);

        // One more entry causes overflow
        leaf.entries.push((9999, RecordId::new(2, 9999)));
        let err = leaf.serialize(&mut page).unwrap_err();
        assert!(matches!(err, Error::ValueTooLarge { .. }));
    }
}
