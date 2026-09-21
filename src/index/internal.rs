use crate::error::{Error, Result};
use crate::index::node::{
    INTERNAL_ENTRY_SIZE, INTERNAL_HEADER_SIZE, INTERNAL_MAX_KEYS, INVALID_PAGE_ID,
};
use crate::storage::page::{Key, Page, PageId, PageType};

/// In-memory representation of a B+ Tree Internal (Routing) Node.
///
/// An internal node stores `n` separator keys and `n + 1` child page pointers.
///
/// # Routing Convention
/// For separators `[K0, K1, ..., Kn-1]` and children `[C0, C1, ..., Cn]`:
/// * `key < K0`       => `C0`
/// * `K0 <= key < K1` => `C1`
/// * `Ki <= key < Ki+1` => `Ci+1`
/// * `key >= Kn-1`    => `Cn`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalNode {
    /// Separator keys sorted in strictly increasing order.
    pub keys: Vec<Key>,
    /// Child page pointers, where `children.len() == keys.len() + 1`.
    pub children: Vec<PageId>,
}

impl InternalNode {
    /// Creates a new internal node with a single initial child and zero separator keys.
    pub fn new(first_child: PageId) -> Self {
        Self {
            keys: Vec::new(),
            children: vec![first_child],
        }
    }

    /// Creates an internal node with explicit keys and children vectors.
    pub fn with_keys_and_children(keys: Vec<Key>, children: Vec<PageId>) -> Result<Self> {
        let node = Self { keys, children };
        node.validate()?;
        Ok(node)
    }

    /// Finds the target child `PageId` to traverse for the given search `key`.
    pub fn find_child(&self, key: Key) -> PageId {
        match self.keys.binary_search(&key) {
            Ok(idx) => self.children[idx + 1],
            Err(idx) => self.children[idx],
        }
    }

    /// Returns the number of separator keys.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Returns the number of child pointers.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Returns true if the internal node has reached maximum separator key capacity.
    pub fn is_full(&self) -> bool {
        self.keys.len() >= INTERNAL_MAX_KEYS
    }

    /// Validates internal node structural invariants.
    pub fn validate(&self) -> Result<()> {
        if self.children.is_empty() {
            return Err(Error::TreeCorruption(
                "internal node must have at least one child pointer".into(),
            ));
        }

        if self.children.len() != self.keys.len() + 1 {
            return Err(Error::TreeCorruption(format!(
                "internal node invariant violated: child count ({}) != key count ({}) + 1",
                self.children.len(),
                self.keys.len()
            )));
        }

        if self.keys.len() > INTERNAL_MAX_KEYS {
            return Err(Error::ValueTooLarge {
                size: self.keys.len(),
                limit: INTERNAL_MAX_KEYS,
            });
        }

        for i in 1..self.keys.len() {
            if self.keys[i] <= self.keys[i - 1] {
                return Err(Error::TreeCorruption(format!(
                    "internal node separator keys not strictly increasing: key {} at index {} <= key {} at index {}",
                    self.keys[i],
                    i,
                    self.keys[i - 1],
                    i - 1
                )));
            }
        }

        for (idx, &child_id) in self.children.iter().enumerate() {
            if child_id == INVALID_PAGE_ID {
                return Err(Error::TreeCorruption(format!(
                    "internal node child at index {idx} contains invalid PageId (0)"
                )));
            }
        }

        Ok(())
    }

    /// Serializes the internal node into the provided `Page` buffer.
    ///
    /// # Layout (Payload bytes 8..4096)
    /// * Offset 0..2: `key_count` (`u16`, LE)
    /// * Offset 2..6: `children[0]` (`u32`, LE)
    /// * Offset 6..: Array of `[SeparatorKey (8B) | ChildPageId (4B)]`
    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        self.validate()?;

        page.set_page_type(PageType::Internal);
        let payload = page.payload_mut();

        // 1. Write header
        let key_count = self.keys.len() as u16;
        payload[0..2].copy_from_slice(&key_count.to_le_bytes());
        payload[2..6].copy_from_slice(&self.children[0].to_le_bytes());

        // 2. Write (Key, Child) entries
        let mut offset = INTERNAL_HEADER_SIZE;
        for i in 0..self.keys.len() {
            payload[offset..offset + 8].copy_from_slice(&self.keys[i].to_le_bytes());
            payload[offset + 8..offset + 12].copy_from_slice(&self.children[i + 1].to_le_bytes());
            offset += INTERNAL_ENTRY_SIZE;
        }

        // Zero-fill remaining payload
        payload[offset..].fill(0);

        Ok(())
    }

    /// Deserializes an `InternalNode` from a `Page`, performing structural validation.
    pub fn deserialize(page: &Page) -> Result<Self> {
        if page.page_type() != PageType::Internal {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "expected Internal page type tag",
            });
        }

        let payload = page.payload();
        let key_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        if key_count > INTERNAL_MAX_KEYS {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "internal key count exceeds maximum capacity",
            });
        }

        let first_child = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
        let mut keys = Vec::with_capacity(key_count);
        let mut children = Vec::with_capacity(key_count + 1);
        children.push(first_child);

        let mut offset = INTERNAL_HEADER_SIZE;
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
            let child = u32::from_le_bytes([
                payload[offset + 8],
                payload[offset + 9],
                payload[offset + 10],
                payload[offset + 11],
            ]);

            if i > 0 && key <= keys[i - 1] {
                return Err(Error::PageCorruption {
                    page_id: page.id(),
                    reason: "internal separator keys are not strictly increasing",
                });
            }

            keys.push(key);
            children.push(child);
            offset += INTERNAL_ENTRY_SIZE;
        }

        let node = Self { keys, children };
        node.validate().map_err(|_e| Error::PageCorruption {
            page_id: page.id(),
            reason: "internal node failed invariant validation",
        })?;

        Ok(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_internal_routing() {
        // Separators [50, 80], children [C0=10, C1=20, C2=30]
        let node = InternalNode::with_keys_and_children(vec![50, 80], vec![10, 20, 30])
            .expect("valid node");

        // Routing checks
        assert_eq!(node.find_child(0), 10);
        assert_eq!(node.find_child(49), 10);
        assert_eq!(node.find_child(50), 20); // exact separator matches right child
        assert_eq!(node.find_child(65), 20);
        assert_eq!(node.find_child(79), 20);
        assert_eq!(node.find_child(80), 30); // exact separator matches right child
        assert_eq!(node.find_child(100), 30);
    }

    #[test]
    fn test_internal_serialization_roundtrip() {
        let node = InternalNode::with_keys_and_children(vec![100, 200, 300], vec![10, 20, 30, 40])
            .expect("valid node");

        let mut page = Page::new(5, PageType::Internal);
        node.serialize(&mut page).expect("serialize");

        let restored = InternalNode::deserialize(&page).expect("deserialize");
        assert_eq!(restored, node);
    }

    #[test]
    fn test_internal_max_capacity() {
        let mut keys = Vec::with_capacity(INTERNAL_MAX_KEYS);
        let mut children = Vec::with_capacity(INTERNAL_MAX_KEYS + 1);

        children.push(1);
        for i in 0..INTERNAL_MAX_KEYS {
            keys.push((i as u64) * 10 + 10);
            children.push((i as u32) + 2);
        }

        let node = InternalNode::with_keys_and_children(keys, children).expect("valid max node");
        assert!(node.is_full());

        let mut page = Page::new(7, PageType::Internal);
        node.serialize(&mut page).expect("serialize max");
        let restored = InternalNode::deserialize(&page).expect("deserialize max");
        assert_eq!(restored.key_count(), INTERNAL_MAX_KEYS);
        assert_eq!(restored.child_count(), INTERNAL_MAX_KEYS + 1);
        assert_eq!(restored, node);
    }

    #[test]
    fn test_internal_unsorted_keys_rejected() {
        let err = InternalNode::with_keys_and_children(vec![200, 100], vec![1, 2, 3]).unwrap_err();
        assert!(matches!(err, Error::TreeCorruption(_)));
    }

    #[test]
    fn test_internal_mismatched_children_rejected() {
        let err = InternalNode::with_keys_and_children(vec![100, 200], vec![1, 2]).unwrap_err();
        assert!(matches!(err, Error::TreeCorruption(_)));
    }
}
