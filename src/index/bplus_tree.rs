use crate::error::{Error, Result};
use crate::index::internal::InternalNode;
use crate::index::leaf::LeafNode;
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{Key, PageId, PageType};

/// In-memory handle representing a disk-backed B+ Tree.
///
/// Coordinates read-only tree traversal and point lookups across internal and
/// leaf nodes via the `BufferPool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BPlusTree {
    root_page_id: PageId,
}

impl BPlusTree {
    /// Creates a B+ Tree handle referencing an existing root page.
    pub const fn new(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    /// Creates a B+ Tree handle from a known root page ID.
    pub const fn from_root(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    /// Creates a B+ Tree handle representing an empty (uninitialized) tree.
    pub const fn new_empty() -> Self {
        Self {
            root_page_id: INVALID_PAGE_ID,
        }
    }

    /// Returns the root page ID of the B+ Tree.
    pub const fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    /// Returns true if the B+ Tree has no root page (empty tree).
    pub const fn is_empty(&self) -> bool {
        self.root_page_id == INVALID_PAGE_ID
    }

    /// Performs a point lookup for `key` in the B+ Tree.
    ///
    /// Traverses internal routing nodes starting from `root_page_id`, locates
    /// the target leaf node, and searches for the corresponding `RecordId`.
    ///
    /// # Pinning Discipline
    /// Guarantees that every page fetched from the `BufferPool` during traversal
    /// is promptly unpinned before descending or returning, preventing frame pin leaks.
    ///
    /// # Returns
    /// * `Ok(Some(RecordId))` if key was found in a leaf node.
    /// * `Ok(None)` if key does not exist or tree is empty.
    /// * `Err(Error)` if I/O, node corruption, or invalid page type was encountered.
    pub fn get(&self, buffer_pool: &mut BufferPool, key: Key) -> Result<Option<RecordId>> {
        if self.is_empty() {
            return Ok(None);
        }

        let mut curr_page_id = self.root_page_id;

        loop {
            let page = buffer_pool.fetch_page(curr_page_id)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    let leaf = match LeafNode::deserialize(page) {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_page_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(curr_page_id, false)?;
                    return Ok(leaf.get(key));
                }
                PageType::Internal => {
                    let internal = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_page_id, false);
                            return Err(e);
                        }
                    };
                    let next_page_id = internal.find_child(key);
                    buffer_pool.unpin_page(curr_page_id, false)?;
                    curr_page_id = next_page_id;
                }
                other => {
                    buffer_pool.unpin_page(curr_page_id, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_page_id} during B+ Tree traversal"
                    )));
                }
            }
        }
    }
}
