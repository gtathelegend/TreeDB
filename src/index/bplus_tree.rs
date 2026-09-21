use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::index::internal::InternalNode;
use crate::index::leaf::LeafNode;
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{Key, PageId, PageType};

/// In-memory handle representing a disk-backed B+ Tree.
///
/// Coordinates tree traversal, point lookups, and persistent insertions across
/// internal and leaf nodes via the `BufferPool`.
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

    /// Inserts a `(Key, RecordId)` pair into the persistent B+ Tree.
    ///
    /// # Insertion Workflow
    /// 1. If tree is empty, allocates a new Leaf root page and inserts the initial entry.
    /// 2. If tree is not empty, traverses down to the target leaf recording the parent page path.
    /// 3. If leaf has capacity, inserts in-place and marks the page dirty.
    /// 4. If leaf is full, splits into two balanced leaves, allocates a new right leaf page,
    ///    links siblings, and propagates the separator key upward.
    /// 5. Recursively inserts the promoted separator into internal parent nodes, splitting
    ///    parents as necessary.
    /// 6. If the root node splits, allocates a new Internal root page, updates `self.root_page_id`,
    ///    and persists the new root in the database header on Page 0.
    ///
    /// # Invariant & Pin Discipline
    /// Guarantees that every page fetched or allocated is correctly unpinned with appropriate
    /// dirty marking, and all B+ Tree structural invariants are preserved.
    ///
    /// # Errors
    /// * `Error::DuplicateKey` if `key` already exists.
    /// * `Error::Io` or `Error::TreeCorruption` on storage or validation errors.
    pub fn insert(
        &mut self,
        buffer_pool: &mut BufferPool,
        key: Key,
        record_id: RecordId,
    ) -> Result<()> {
        if self.is_empty() {
            // Case 1: Empty Tree -> Allocate first leaf root
            let root_page_id = buffer_pool.allocate_page(PageType::Leaf)?;
            let mut leaf = LeafNode::new();
            leaf.entries.push((key, record_id));

            {
                let page_mut = buffer_pool.get_page_mut(root_page_id)?;
                leaf.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(root_page_id, true)?;

            self.root_page_id = root_page_id;
            buffer_pool
                .page_manager_mut()
                .set_root_page_id(root_page_id)?;

            return Ok(());
        }

        // Case 2: Non-empty tree -> Traverse to leaf while saving path stack of internal nodes
        let mut path: Vec<PageId> = Vec::new();
        let mut curr_page_id = self.root_page_id;

        loop {
            let page = buffer_pool.fetch_page(curr_page_id)?;
            let page_type = page.page_type();

            match page_type {
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
                    path.push(curr_page_id);
                    curr_page_id = next_page_id;
                }
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_page_id, false)?;
                    break;
                }
                other => {
                    buffer_pool.unpin_page(curr_page_id, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_page_id} during insertion traversal"
                    )));
                }
            }
        }

        let target_leaf_id = curr_page_id;

        // Fetch leaf for reading & deserialize
        let mut leaf = {
            let page = buffer_pool.fetch_page(target_leaf_id)?;
            let l = match LeafNode::deserialize(page) {
                Ok(l) => l,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(target_leaf_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(target_leaf_id, false)?;
            l
        };

        // Check for duplicate key
        if leaf.get(key).is_some() {
            return Err(Error::DuplicateKey(key));
        }

        let mut split_to_propagate: Option<(Key, PageId)> = None;

        if !leaf.is_full() {
            // Fits in leaf without split
            leaf.insert(key, record_id)?;
            {
                let page_mut = buffer_pool.fetch_page_mut(target_leaf_id)?;
                leaf.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(target_leaf_id, true)?;
        } else {
            // Leaf is full -> Split leaf
            let right_leaf_id = buffer_pool.allocate_page(PageType::Leaf)?;
            let (sep_key, right_leaf) = leaf.split_insert(key, record_id, right_leaf_id)?;

            // Serialize updated left leaf
            {
                let left_page_mut = buffer_pool.fetch_page_mut(target_leaf_id)?;
                leaf.serialize(left_page_mut)?;
            }
            buffer_pool.unpin_page(target_leaf_id, true)?;

            // Serialize new right leaf
            {
                let right_page_mut = buffer_pool.get_page_mut(right_leaf_id)?;
                right_leaf.serialize(right_page_mut)?;
            }
            buffer_pool.unpin_page(right_leaf_id, true)?;

            split_to_propagate = Some((sep_key, right_leaf_id));
        }

        // Propagate splits upward through path stack
        while let Some((sep_key, right_child_id)) = split_to_propagate {
            if let Some(parent_id) = path.pop() {
                let mut parent_node = {
                    let parent_page = buffer_pool.fetch_page(parent_id)?;
                    let n = match InternalNode::deserialize(parent_page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(parent_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(parent_id, false)?;
                    n
                };

                if !parent_node.is_full() {
                    parent_node.insert(sep_key, right_child_id)?;
                    {
                        let parent_page_mut = buffer_pool.fetch_page_mut(parent_id)?;
                        parent_node.serialize(parent_page_mut)?;
                    }
                    buffer_pool.unpin_page(parent_id, true)?;
                    split_to_propagate = None;
                } else {
                    // Parent node is full -> Split internal node
                    let right_internal_id = buffer_pool.allocate_page(PageType::Internal)?;
                    let (promoted_key, right_internal_node) =
                        parent_node.split_insert(sep_key, right_child_id)?;

                    // Serialize updated left parent node
                    {
                        let left_parent_mut = buffer_pool.fetch_page_mut(parent_id)?;
                        parent_node.serialize(left_parent_mut)?;
                    }
                    buffer_pool.unpin_page(parent_id, true)?;

                    // Serialize new right parent node
                    {
                        let right_int_page_mut = buffer_pool.get_page_mut(right_internal_id)?;
                        right_internal_node.serialize(right_int_page_mut)?;
                    }
                    buffer_pool.unpin_page(right_internal_id, true)?;

                    split_to_propagate = Some((promoted_key, right_internal_id));
                }
            } else {
                // Root itself split! Create a new root internal node
                let new_root_id = buffer_pool.allocate_page(PageType::Internal)?;
                let new_root_node = InternalNode::with_keys_and_children(
                    vec![sep_key],
                    vec![self.root_page_id, right_child_id],
                )?;

                {
                    let new_root_page = buffer_pool.get_page_mut(new_root_id)?;
                    new_root_node.serialize(new_root_page)?;
                }
                buffer_pool.unpin_page(new_root_id, true)?;

                self.root_page_id = new_root_id;
                buffer_pool
                    .page_manager_mut()
                    .set_root_page_id(new_root_id)?;

                split_to_propagate = None;
            }
        }

        Ok(())
    }

    /// Recursively validates all structural invariants of the B+ Tree on disk.
    ///
    /// Verifies:
    /// 1. All leaves are at the exact same depth.
    /// 2. All node keys are strictly increasing.
    /// 3. All internal nodes satisfy `children.len() == keys.len() + 1`.
    /// 4. Routing boundaries match key ranges recursively.
    /// 5. Leaf sibling chain (`next_leaf`) is sorted and contains all leaves exactly once without cycles.
    pub fn validate_tree(&self, buffer_pool: &mut BufferPool) -> Result<()> {
        if self.is_empty() {
            return Ok(());
        }

        let mut expected_leaf_depth: Option<usize> = None;
        let mut leaf_pages_seen = HashSet::new();

        self.validate_node_recursive(
            buffer_pool,
            self.root_page_id,
            0,
            &mut expected_leaf_depth,
            &mut leaf_pages_seen,
            None,
            None,
        )?;

        // Validate leaf sibling chain
        let mut curr_leaf_id = self.find_leftmost_leaf(buffer_pool)?;
        let mut visited_chain = HashSet::new();
        let mut prev_max_key: Option<Key> = None;

        while curr_leaf_id != INVALID_PAGE_ID {
            if !visited_chain.insert(curr_leaf_id) {
                return Err(Error::TreeCorruption(format!(
                    "cycle detected in leaf sibling chain at page {curr_leaf_id}"
                )));
            }

            let page = buffer_pool.fetch_page(curr_leaf_id)?;
            let leaf = match LeafNode::deserialize(page) {
                Ok(l) => l,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_leaf_id, false);
                    return Err(e);
                }
            };

            if let Some(prev_max) = prev_max_key {
                if let Some(first_entry) = leaf.entries.first() {
                    if first_entry.0 <= prev_max {
                        let _ = buffer_pool.unpin_page(curr_leaf_id, false);
                        return Err(Error::TreeCorruption(format!(
                            "leaf sibling chain order violation: key {} in leaf {} <= previous max key {}",
                            first_entry.0, curr_leaf_id, prev_max
                        )));
                    }
                }
            }

            if let Some(last_entry) = leaf.entries.last() {
                prev_max_key = Some(last_entry.0);
            }

            let next_leaf = leaf.next_leaf;
            buffer_pool.unpin_page(curr_leaf_id, false)?;
            curr_leaf_id = next_leaf;
        }

        if visited_chain.len() != leaf_pages_seen.len() {
            return Err(Error::TreeCorruption(format!(
                "leaf sibling chain count ({}) != traversed leaf count ({})",
                visited_chain.len(),
                leaf_pages_seen.len()
            )));
        }

        Ok(())
    }

    /// Finds the leftmost leaf page ID in the tree.
    fn find_leftmost_leaf(&self, buffer_pool: &mut BufferPool) -> Result<PageId> {
        let mut curr = self.root_page_id;
        loop {
            let page = buffer_pool.fetch_page(curr)?;
            match page.page_type() {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr, false)?;
                    return Ok(curr);
                }
                PageType::Internal => {
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr, false);
                            return Err(e);
                        }
                    };
                    let first_child = node.children[0];
                    buffer_pool.unpin_page(curr, false)?;
                    curr = first_child;
                }
                other => {
                    let _ = buffer_pool.unpin_page(curr, false);
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr}"
                    )));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_node_recursive(
        &self,
        buffer_pool: &mut BufferPool,
        page_id: PageId,
        depth: usize,
        expected_leaf_depth: &mut Option<usize>,
        leaf_pages_seen: &mut HashSet<PageId>,
        lower_bound: Option<Key>,
        upper_bound: Option<Key>,
    ) -> Result<()> {
        let page = buffer_pool.fetch_page(page_id)?;
        let page_type = page.page_type();

        match page_type {
            PageType::Leaf => {
                let leaf = match LeafNode::deserialize(page) {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(page_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(page_id, false)?;

                leaf_pages_seen.insert(page_id);

                if let Some(expected) = *expected_leaf_depth {
                    if depth != expected {
                        return Err(Error::TreeCorruption(format!(
                            "leaf at page {page_id} has depth {depth}, expected {expected}"
                        )));
                    }
                } else {
                    *expected_leaf_depth = Some(depth);
                }

                // Check bounds against parent separators
                for &(k, _) in &leaf.entries {
                    if let Some(lb) = lower_bound {
                        if k < lb {
                            return Err(Error::TreeCorruption(format!(
                                "leaf key {k} is below lower bound {lb}"
                            )));
                        }
                    }
                    if let Some(ub) = upper_bound {
                        if k >= ub {
                            return Err(Error::TreeCorruption(format!(
                                "leaf key {k} is above/equal upper bound {ub}"
                            )));
                        }
                    }
                }

                Ok(())
            }
            PageType::Internal => {
                let internal = match InternalNode::deserialize(page) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(page_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(page_id, false)?;

                // Recursively validate each child branch with updated bounds
                for i in 0..internal.children.len() {
                    let child_id = internal.children[i];

                    let child_lb = if i == 0 {
                        lower_bound
                    } else {
                        Some(internal.keys[i - 1])
                    };

                    let child_ub = if i < internal.keys.len() {
                        Some(internal.keys[i])
                    } else {
                        upper_bound
                    };

                    self.validate_node_recursive(
                        buffer_pool,
                        child_id,
                        depth + 1,
                        expected_leaf_depth,
                        leaf_pages_seen,
                        child_lb,
                        child_ub,
                    )?;
                }

                Ok(())
            }
            other => {
                buffer_pool.unpin_page(page_id, false)?;
                Err(Error::TreeCorruption(format!(
                    "invalid page type {other:?} at page {page_id}"
                )))
            }
        }
    }
}
