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

    /// Deletes a key from the B+ Tree.
    ///
    /// # Semantics
    /// * If the key exists: removes it, rebalances the tree as necessary (borrowing or merging),
    ///   and returns `Ok(Some(old_record_id))`.
    /// * If the key does not exist: returns `Ok(None)` without modifying the tree.
    ///
    /// # Rebalancing Policy
    /// * Leaves underflow if `entries.len() < LEAF_MIN_ENTRIES` (127).
    /// * Internal nodes underflow if `keys.len() < INTERNAL_MIN_KEYS` (170).
    /// * The root node is exempt from minimum occupancy rules.
    /// * Redistribution (borrowing from left sibling first, then right sibling) is preferred over merging.
    /// * If neither sibling can donate, merging is performed (with left sibling preferred).
    /// * Internal nodes rotate separator keys through the parent during borrowing and demote parent separators during merging.
    /// * Root collapse: An internal root with a single child collapses to that child. A leaf root that becomes empty sets the root to `INVALID_PAGE_ID`.
    pub fn delete(&mut self, buffer_pool: &mut BufferPool, key: Key) -> Result<Option<RecordId>> {
        if self.is_empty() {
            return Ok(None);
        }

        // 1. Traverse to target leaf, recording the path of internal node page IDs.
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
                        "unexpected page type {other:?} at page {curr_page_id} during delete traversal"
                    )));
                }
            }
        }

        let target_leaf_id = curr_page_id;

        // 2. Fetch and deserialize leaf
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

        // 3. Search and remove key
        let old_record_id = match leaf.remove(key) {
            Some(rid) => rid,
            None => return Ok(None),
        };

        // 4. Handle Root Leaf (path is empty)
        if path.is_empty() {
            if leaf.is_empty() {
                // Leaf root is empty -> tree becomes empty
                self.root_page_id = INVALID_PAGE_ID;
                buffer_pool
                    .page_manager_mut()
                    .set_root_page_id(INVALID_PAGE_ID)?;
            } else {
                let page_mut = buffer_pool.fetch_page_mut(target_leaf_id)?;
                leaf.serialize(page_mut)?;
                buffer_pool.unpin_page(target_leaf_id, true)?;
            }
            return Ok(Some(old_record_id));
        }

        // 5. Leaf is not root -> Check underflow
        if !leaf.is_underfull() {
            let page_mut = buffer_pool.fetch_page_mut(target_leaf_id)?;
            leaf.serialize(page_mut)?;
            buffer_pool.unpin_page(target_leaf_id, true)?;
            return Ok(Some(old_record_id));
        }

        // 6. Leaf underflowed! Rebalance with sibling
        let parent_id = *path.last().unwrap();
        let mut parent = {
            let p = buffer_pool.fetch_page(parent_id)?;
            let n = match InternalNode::deserialize(p) {
                Ok(n) => n,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(parent_id, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(parent_id, false)?;
            n
        };

        let child_idx = parent
            .children
            .iter()
            .position(|&c| c == target_leaf_id)
            .ok_or_else(|| {
                Error::TreeCorruption(format!(
                    "parent {parent_id} does not contain child leaf {target_leaf_id}"
                ))
            })?;

        // 6a. Try borrow from left sibling
        if child_idx > 0 {
            let left_sibling_id = parent.children[child_idx - 1];
            let mut left_sibling = {
                let p = buffer_pool.fetch_page(left_sibling_id)?;
                let n = match LeafNode::deserialize(p) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(left_sibling_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(left_sibling_id, false)?;
                n
            };

            if left_sibling.can_donate() {
                let donated = left_sibling.entries.pop().unwrap();
                leaf.entries.insert(0, donated);
                parent.keys[child_idx - 1] = leaf.entries[0].0;

                {
                    let p = buffer_pool.fetch_page_mut(left_sibling_id)?;
                    left_sibling.serialize(p)?;
                }
                buffer_pool.unpin_page(left_sibling_id, true)?;

                {
                    let p = buffer_pool.fetch_page_mut(target_leaf_id)?;
                    leaf.serialize(p)?;
                }
                buffer_pool.unpin_page(target_leaf_id, true)?;

                {
                    let p = buffer_pool.fetch_page_mut(parent_id)?;
                    parent.serialize(p)?;
                }
                buffer_pool.unpin_page(parent_id, true)?;

                return Ok(Some(old_record_id));
            }
        }

        // 6b. Try borrow from right sibling
        if child_idx + 1 < parent.children.len() {
            let right_sibling_id = parent.children[child_idx + 1];
            let mut right_sibling = {
                let p = buffer_pool.fetch_page(right_sibling_id)?;
                let n = match LeafNode::deserialize(p) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(right_sibling_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(right_sibling_id, false)?;
                n
            };

            if right_sibling.can_donate() {
                let donated = right_sibling.entries.remove(0);
                leaf.entries.push(donated);
                parent.keys[child_idx] = right_sibling.entries[0].0;

                {
                    let p = buffer_pool.fetch_page_mut(right_sibling_id)?;
                    right_sibling.serialize(p)?;
                }
                buffer_pool.unpin_page(right_sibling_id, true)?;

                {
                    let p = buffer_pool.fetch_page_mut(target_leaf_id)?;
                    leaf.serialize(p)?;
                }
                buffer_pool.unpin_page(target_leaf_id, true)?;

                {
                    let p = buffer_pool.fetch_page_mut(parent_id)?;
                    parent.serialize(p)?;
                }
                buffer_pool.unpin_page(parent_id, true)?;

                return Ok(Some(old_record_id));
            }
        }

        // 6c. Neither sibling can donate -> MERGE!
        if child_idx > 0 {
            // Merge target_leaf into left sibling
            let left_sibling_id = parent.children[child_idx - 1];
            let mut left_sibling = {
                let p = buffer_pool.fetch_page(left_sibling_id)?;
                let n = match LeafNode::deserialize(p) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(left_sibling_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(left_sibling_id, false)?;
                n
            };

            left_sibling.entries.extend(leaf.entries);
            left_sibling.next_leaf = leaf.next_leaf;

            parent.keys.remove(child_idx - 1);
            parent.children.remove(child_idx);

            {
                let p = buffer_pool.fetch_page_mut(left_sibling_id)?;
                left_sibling.serialize(p)?;
            }
            buffer_pool.unpin_page(left_sibling_id, true)?;
        } else {
            // Merge right sibling into target_leaf
            let right_sibling_id = parent.children[1];
            let right_sibling = {
                let p = buffer_pool.fetch_page(right_sibling_id)?;
                let n = match LeafNode::deserialize(p) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(right_sibling_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(right_sibling_id, false)?;
                n
            };

            leaf.entries.extend(right_sibling.entries);
            leaf.next_leaf = right_sibling.next_leaf;

            parent.keys.remove(0);
            parent.children.remove(1);

            {
                let p = buffer_pool.fetch_page_mut(target_leaf_id)?;
                leaf.serialize(p)?;
            }
            buffer_pool.unpin_page(target_leaf_id, true)?;
        }

        // Serialize parent
        {
            let p = buffer_pool.fetch_page_mut(parent_id)?;
            parent.serialize(p)?;
        }
        buffer_pool.unpin_page(parent_id, true)?;

        // 7. Recursively rebalance internal ancestors
        self.rebalance_internal_path(buffer_pool, path)?;

        Ok(Some(old_record_id))
    }

    /// Recursively rebalances internal nodes along the path from parent toward the root.
    fn rebalance_internal_path(
        &mut self,
        buffer_pool: &mut BufferPool,
        mut path: Vec<PageId>,
    ) -> Result<()> {
        while let Some(curr_id) = path.pop() {
            if curr_id == self.root_page_id {
                // Root is an internal node. Check for root collapse:
                let root_node = {
                    let page = buffer_pool.fetch_page(curr_id)?;
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(curr_id, false)?;
                    node
                };

                if root_node.keys.is_empty() {
                    // Internal root has 0 keys and 1 child -> collapse root!
                    let new_root_id = root_node.children[0];
                    self.root_page_id = new_root_id;
                    buffer_pool
                        .page_manager_mut()
                        .set_root_page_id(new_root_id)?;
                }
                return Ok(());
            }

            let mut curr_node = {
                let page = buffer_pool.fetch_page(curr_id)?;
                let node = match InternalNode::deserialize(page) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(curr_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(curr_id, false)?;
                node
            };

            if !curr_node.is_underfull() {
                // Node meets minimum occupancy -> rebalancing complete!
                return Ok(());
            }

            // Node is underfull! Fetch grandparent to access siblings
            let grandparent_id = *path.last().unwrap();
            let mut grandparent = {
                let page = buffer_pool.fetch_page(grandparent_id)?;
                let node = match InternalNode::deserialize(page) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(grandparent_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(grandparent_id, false)?;
                node
            };

            let p_idx = grandparent
                .children
                .iter()
                .position(|&c| c == curr_id)
                .ok_or_else(|| {
                    Error::TreeCorruption(format!(
                        "grandparent {grandparent_id} does not contain child internal node {curr_id}"
                    ))
                })?;

            // Try borrow from left internal sibling
            if p_idx > 0 {
                let left_sibling_id = grandparent.children[p_idx - 1];
                let mut left_sibling = {
                    let page = buffer_pool.fetch_page(left_sibling_id)?;
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(left_sibling_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(left_sibling_id, false)?;
                    node
                };

                if left_sibling.can_donate() {
                    let demoted_key = grandparent.keys[p_idx - 1];
                    let donated_child = left_sibling.children.pop().unwrap();
                    let donated_key = left_sibling.keys.pop().unwrap();

                    grandparent.keys[p_idx - 1] = donated_key;
                    curr_node.keys.insert(0, demoted_key);
                    curr_node.children.insert(0, donated_child);

                    {
                        let p = buffer_pool.fetch_page_mut(left_sibling_id)?;
                        left_sibling.serialize(p)?;
                    }
                    buffer_pool.unpin_page(left_sibling_id, true)?;

                    {
                        let p = buffer_pool.fetch_page_mut(curr_id)?;
                        curr_node.serialize(p)?;
                    }
                    buffer_pool.unpin_page(curr_id, true)?;

                    {
                        let p = buffer_pool.fetch_page_mut(grandparent_id)?;
                        grandparent.serialize(p)?;
                    }
                    buffer_pool.unpin_page(grandparent_id, true)?;

                    return Ok(());
                }
            }

            // Try borrow from right internal sibling
            if p_idx + 1 < grandparent.children.len() {
                let right_sibling_id = grandparent.children[p_idx + 1];
                let mut right_sibling = {
                    let page = buffer_pool.fetch_page(right_sibling_id)?;
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(right_sibling_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(right_sibling_id, false)?;
                    node
                };

                if right_sibling.can_donate() {
                    let demoted_key = grandparent.keys[p_idx];
                    let donated_child = right_sibling.children.remove(0);
                    let donated_key = right_sibling.keys.remove(0);

                    grandparent.keys[p_idx] = donated_key;
                    curr_node.keys.push(demoted_key);
                    curr_node.children.push(donated_child);

                    {
                        let p = buffer_pool.fetch_page_mut(right_sibling_id)?;
                        right_sibling.serialize(p)?;
                    }
                    buffer_pool.unpin_page(right_sibling_id, true)?;

                    {
                        let p = buffer_pool.fetch_page_mut(curr_id)?;
                        curr_node.serialize(p)?;
                    }
                    buffer_pool.unpin_page(curr_id, true)?;

                    {
                        let p = buffer_pool.fetch_page_mut(grandparent_id)?;
                        grandparent.serialize(p)?;
                    }
                    buffer_pool.unpin_page(grandparent_id, true)?;

                    return Ok(());
                }
            }

            // Neither sibling can donate -> MERGE Internal Nodes!
            if p_idx > 0 {
                // Merge curr_node into left sibling
                let left_sibling_id = grandparent.children[p_idx - 1];
                let mut left_sibling = {
                    let page = buffer_pool.fetch_page(left_sibling_id)?;
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(left_sibling_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(left_sibling_id, false)?;
                    node
                };

                let demoted_sep = grandparent.keys.remove(p_idx - 1);
                grandparent.children.remove(p_idx);

                left_sibling.keys.push(demoted_sep);
                left_sibling.keys.extend(curr_node.keys);
                left_sibling.children.extend(curr_node.children);

                {
                    let p = buffer_pool.fetch_page_mut(left_sibling_id)?;
                    left_sibling.serialize(p)?;
                }
                buffer_pool.unpin_page(left_sibling_id, true)?;
            } else {
                // Merge right sibling into curr_node
                let right_sibling_id = grandparent.children[1];
                let right_sibling = {
                    let page = buffer_pool.fetch_page(right_sibling_id)?;
                    let node = match InternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(right_sibling_id, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(right_sibling_id, false)?;
                    node
                };

                let demoted_sep = grandparent.keys.remove(0);
                grandparent.children.remove(1);

                curr_node.keys.push(demoted_sep);
                curr_node.keys.extend(right_sibling.keys);
                curr_node.children.extend(right_sibling.children);

                {
                    let p = buffer_pool.fetch_page_mut(curr_id)?;
                    curr_node.serialize(p)?;
                }
                buffer_pool.unpin_page(curr_id, true)?;
            }

            // Save updated grandparent
            {
                let p = buffer_pool.fetch_page_mut(grandparent_id)?;
                grandparent.serialize(p)?;
            }
            buffer_pool.unpin_page(grandparent_id, true)?;
        }

        // Finally check root collapse if root was modified
        if self.root_page_id != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(self.root_page_id)?;
            if page.page_type() == PageType::Internal {
                let root_node = match InternalNode::deserialize(page) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = buffer_pool.unpin_page(self.root_page_id, false);
                        return Err(e);
                    }
                };
                buffer_pool.unpin_page(self.root_page_id, false)?;

                if root_node.keys.is_empty() {
                    let new_root = root_node.children[0];
                    self.root_page_id = new_root;
                    buffer_pool.page_manager_mut().set_root_page_id(new_root)?;
                }
            } else {
                buffer_pool.unpin_page(self.root_page_id, false)?;
            }
        }

        Ok(())
    }

    /// Performs an ordered range scan over keys in `[start, end)`.
    ///
    /// # Range Semantics
    /// * `start`: Inclusive lower bound, or `None` to start from the minimum key in the tree.
    /// * `end`: Exclusive upper bound, or `None` to scan up to the maximum key in the tree.
    /// * If `start >= end` (when both are `Some`), returns an empty `Vec`.
    /// * Sequentially traverses leaf nodes using their `next_leaf` sibling pointers.
    ///
    /// # Pinning Discipline
    /// Guarantees that each leaf page is promptly unpinned before advancing to `next_leaf`
    /// or returning, holding zero Buffer Pool pins on exit.
    pub fn scan(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<Key>,
        end: Option<Key>,
    ) -> Result<Vec<(Key, RecordId)>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }

        if let (Some(s), Some(e)) = (start, end) {
            if s >= e {
                return Ok(Vec::new());
            }
        }

        // Find the starting leaf page and entry offset
        let (mut curr_leaf_id, mut start_idx) = match start {
            Some(start_key) => {
                let mut curr_page_id = self.root_page_id;
                loop {
                    let page = buffer_pool.fetch_page(curr_page_id)?;
                    match page.page_type() {
                        PageType::Internal => {
                            let internal = match InternalNode::deserialize(page) {
                                Ok(n) => n,
                                Err(e) => {
                                    let _ = buffer_pool.unpin_page(curr_page_id, false);
                                    return Err(e);
                                }
                            };
                            let next_id = internal.find_child(start_key);
                            buffer_pool.unpin_page(curr_page_id, false)?;
                            curr_page_id = next_id;
                        }
                        PageType::Leaf => {
                            let leaf = match LeafNode::deserialize(page) {
                                Ok(l) => l,
                                Err(e) => {
                                    let _ = buffer_pool.unpin_page(curr_page_id, false);
                                    return Err(e);
                                }
                            };
                            buffer_pool.unpin_page(curr_page_id, false)?;
                            let idx =
                                match leaf.entries.binary_search_by_key(&start_key, |&(k, _)| k) {
                                    Ok(i) => i,
                                    Err(i) => i,
                                };
                            break (curr_page_id, idx);
                        }
                        other => {
                            let _ = buffer_pool.unpin_page(curr_page_id, false);
                            return Err(Error::TreeCorruption(format!(
                                "unexpected page type {other:?} at page {curr_page_id} during scan traversal"
                            )));
                        }
                    }
                }
            }
            None => (self.find_leftmost_leaf(buffer_pool)?, 0),
        };

        let mut results = Vec::new();

        while curr_leaf_id != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(curr_leaf_id)?;
            let leaf = match LeafNode::deserialize(page) {
                Ok(l) => l,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(curr_leaf_id, false);
                    return Err(e);
                }
            };

            for &(k, rid) in &leaf.entries[start_idx..] {
                if let Some(end_key) = end {
                    if k >= end_key {
                        buffer_pool.unpin_page(curr_leaf_id, false)?;
                        return Ok(results);
                    }
                }
                results.push((k, rid));
            }

            let next_leaf = leaf.next_leaf;
            buffer_pool.unpin_page(curr_leaf_id, false)?;
            curr_leaf_id = next_leaf;
            start_idx = 0;
        }

        Ok(results)
    }

    /// Recursively validates all structural invariants of the B+ Tree on disk.
    ///
    /// Verifies:
    /// 1. All leaves are at the exact same depth.
    /// 2. All node keys are strictly increasing.
    /// 3. All internal nodes satisfy `children.len() == keys.len() + 1`.
    /// 4. Routing boundaries match key ranges recursively.
    /// 5. Leaf sibling chain (`next_leaf`) is sorted and contains all leaves exactly once without cycles.
    /// 6. Non-root nodes respect minimum occupancy invariants.
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

                // Check minimum occupancy for non-root leaf nodes
                if depth > 0 && leaf.is_underfull() {
                    return Err(Error::TreeCorruption(format!(
                        "non-root leaf node {page_id} is underfull with {} entries",
                        leaf.entries.len()
                    )));
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

                // Check minimum occupancy for non-root internal nodes
                if depth > 0 && internal.is_underfull() {
                    return Err(Error::TreeCorruption(format!(
                        "non-root internal node {page_id} is underfull with {} keys",
                        internal.keys.len()
                    )));
                }

                // Check root internal node has >= 1 key
                if depth == 0 && internal.keys.is_empty() {
                    return Err(Error::TreeCorruption(format!(
                        "root internal node {page_id} has 0 keys"
                    )));
                }

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
