use std::collections::HashSet;

use crate::engine::types::{DataType, Value};
use crate::error::{Error, Result};
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{Page, PageId, PageType, PAGE_PAYLOAD_SIZE};

/// Canonical, order-preserving encoded key for secondary B+ Tree indexing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexKey {
    pub bytes: Vec<u8>,
}

impl IndexKey {
    /// Creates an `IndexKey` from raw canonical bytes.
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Returns a slice of the canonical key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Encodes a typed `Value` into a canonical, order-preserving `IndexKey`.
    ///
    /// # Canonical Encoding Rules
    /// - `Null`: Tag `0` (0 bytes payload)
    /// - `Boolean(false)`: Tag `1` (0 bytes payload)
    /// - `Boolean(true)`: Tag `2` (0 bytes payload)
    /// - `Int64(i)`: Tag `3` + 8 bytes big-endian with sign bit flipped `(i as u64 ^ (1 << 63))`
    /// - `UInt64(u)`: Tag `4` + 8 bytes big-endian `u`
    /// - `Float64(f)`: Tag `5` + 8 bytes big-endian order-preserving IEEE bits
    /// - `String(s)`: Tag `6` + UTF-8 payload bytes
    /// - `Bytes(b)`: Tag `7` + raw payload bytes
    pub fn from_value(value: &Value) -> Self {
        let mut bytes = Vec::new();
        match value {
            Value::Null => {
                bytes.push(0);
            }
            Value::Boolean(false) => {
                bytes.push(1);
            }
            Value::Boolean(true) => {
                bytes.push(2);
            }
            Value::Int64(i) => {
                bytes.push(3);
                let sortable = (*i as u64) ^ (1u64 << 63);
                bytes.extend_from_slice(&sortable.to_be_bytes());
            }
            Value::UInt64(u) => {
                bytes.push(4);
                bytes.extend_from_slice(&u.to_be_bytes());
            }
            Value::Float64(f) => {
                bytes.push(5);
                let normalized = if *f == 0.0 { 0.0f64 } else { *f };
                let bits = normalized.to_bits();
                let sortable = if (bits & (1u64 << 63)) != 0 {
                    !bits
                } else {
                    bits ^ (1u64 << 63)
                };
                bytes.extend_from_slice(&sortable.to_be_bytes());
            }
            Value::String(s) => {
                bytes.push(6);
                bytes.extend_from_slice(s.as_bytes());
            }
            Value::Bytes(b) => {
                bytes.push(7);
                bytes.extend_from_slice(b);
            }
        }
        Self { bytes }
    }

    /// Decodes this `IndexKey` back into a typed `Value` given the column's `DataType`.
    pub fn to_value(&self, data_type: DataType) -> Result<Value> {
        if self.bytes.is_empty() {
            return Err(Error::IndexCorruption(
                "empty index key byte payload".to_string(),
            ));
        }

        let tag = self.bytes[0];
        if tag == 0 {
            return Ok(Value::Null);
        }

        match (tag, data_type) {
            (1, DataType::Boolean) => Ok(Value::Boolean(false)),
            (2, DataType::Boolean) => Ok(Value::Boolean(true)),
            (3, DataType::Int64) => {
                if self.bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid Int64 index key length {}",
                        self.bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&self.bytes[1..9]);
                let sortable = u64::from_be_bytes(buf);
                let original = (sortable ^ (1u64 << 63)) as i64;
                Ok(Value::Int64(original))
            }
            (4, DataType::UInt64) => {
                if self.bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid UInt64 index key length {}",
                        self.bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&self.bytes[1..9]);
                let u = u64::from_be_bytes(buf);
                Ok(Value::UInt64(u))
            }
            (5, DataType::Float64) => {
                if self.bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid Float64 index key length {}",
                        self.bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&self.bytes[1..9]);
                let sortable = u64::from_be_bytes(buf);
                let bits = if (sortable & (1u64 << 63)) != 0 {
                    sortable ^ (1u64 << 63)
                } else {
                    !sortable
                };
                let f = f64::from_bits(bits);
                Ok(Value::Float64(f))
            }
            (6, DataType::String) => {
                let s = std::str::from_utf8(&self.bytes[1..]).map_err(|_| {
                    Error::IndexCorruption("invalid UTF-8 in String index key".to_string())
                })?;
                Ok(Value::String(s.to_string()))
            }
            (7, DataType::Bytes) => Ok(Value::Bytes(self.bytes[1..].to_vec())),
            _ => Err(Error::IndexCorruption(format!(
                "index key tag {tag} does not match expected data type {data_type:?}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Secondary B+ Tree Leaf Node
// ---------------------------------------------------------------------------

pub const SEC_LEAF_HEADER_SIZE: usize = 8;
pub const SEC_NODE_MAX_PAYLOAD_BYTES: usize = 3800;

/// Leaf node in a secondary B+ Tree index storing sorted `(IndexKey, RecordId)` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryLeafNode {
    pub next_leaf: PageId,
    pub entries: Vec<(IndexKey, RecordId)>,
}

impl Default for SecondaryLeafNode {
    fn default() -> Self {
        Self::new()
    }
}

impl SecondaryLeafNode {
    pub fn new() -> Self {
        Self {
            next_leaf: INVALID_PAGE_ID,
            entries: Vec::new(),
        }
    }

    pub fn byte_size(&self) -> usize {
        SEC_LEAF_HEADER_SIZE
            + self
                .entries
                .iter()
                .map(|(k, _)| 2 + k.bytes.len() + 8)
                .sum::<usize>()
    }

    pub fn is_full(&self) -> bool {
        self.byte_size() >= SEC_NODE_MAX_PAYLOAD_BYTES || self.entries.len() >= 200
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        let payload = page.payload_mut();
        payload[0..2].copy_from_slice(&(self.entries.len() as u16).to_le_bytes());
        payload[2..6].copy_from_slice(&self.next_leaf.to_le_bytes());
        payload[6..8].fill(0); // reserved

        let mut offset = SEC_LEAF_HEADER_SIZE;
        for (key, rid) in &self.entries {
            let klen = key.bytes.len();
            if offset + 2 + klen + 8 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "secondary leaf page {} overflowed payload capacity",
                    page.id()
                )));
            }
            payload[offset..offset + 2].copy_from_slice(&(klen as u16).to_le_bytes());
            offset += 2;
            payload[offset..offset + klen].copy_from_slice(&key.bytes);
            offset += klen;
            payload[offset..offset + 8].copy_from_slice(&rid.to_bytes());
            offset += 8;
        }

        if offset < PAGE_PAYLOAD_SIZE {
            payload[offset..PAGE_PAYLOAD_SIZE].fill(0);
        }

        Ok(())
    }

    pub fn deserialize(page: &Page) -> Result<Self> {
        let payload = page.payload();
        let entry_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let next_leaf = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);

        let mut entries = Vec::with_capacity(entry_count);
        let mut offset = SEC_LEAF_HEADER_SIZE;

        for _ in 0..entry_count {
            if offset + 2 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "unexpected end of payload on secondary leaf page {}",
                    page.id()
                )));
            }
            let klen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2;

            if offset + klen + 8 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "unexpected end of payload reading entry on secondary leaf page {}",
                    page.id()
                )));
            }
            let key_bytes = payload[offset..offset + klen].to_vec();
            offset += klen;

            let mut rid_bytes = [0u8; 8];
            rid_bytes.copy_from_slice(&payload[offset..offset + 8]);
            offset += 8;
            let rid = RecordId::from_bytes(rid_bytes);

            entries.push((IndexKey::new(key_bytes), rid));
        }

        Ok(Self { next_leaf, entries })
    }

    pub fn insert(&mut self, key: IndexKey, rid: RecordId) -> Result<()> {
        let target = (key, rid);
        match self.entries.binary_search(&target) {
            Ok(_) => Ok(()), // identical entry already exists
            Err(idx) => {
                self.entries.insert(idx, target);
                Ok(())
            }
        }
    }

    pub fn remove(&mut self, key: &IndexKey, rid: RecordId) -> bool {
        let target = (key.clone(), rid);
        if let Ok(idx) = self.entries.binary_search(&target) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn split(&mut self, new_page_id: PageId) -> (SecondaryLeafNode, IndexKey) {
        let mid = self.entries.len() / 2;
        let right_entries = self.entries.split_off(mid);
        let split_key = right_entries[0].0.clone();

        let right_leaf = SecondaryLeafNode {
            next_leaf: self.next_leaf,
            entries: right_entries,
        };
        self.next_leaf = new_page_id;

        (right_leaf, split_key)
    }
}

// ---------------------------------------------------------------------------
// Secondary B+ Tree Internal Node
// ---------------------------------------------------------------------------

pub const SEC_INTERNAL_HEADER_SIZE: usize = 8;

/// Internal routing node in a secondary B+ Tree index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryInternalNode {
    pub keys: Vec<IndexKey>,
    pub children: Vec<PageId>,
}

impl Default for SecondaryInternalNode {
    fn default() -> Self {
        Self::new()
    }
}

impl SecondaryInternalNode {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            children: Vec::new(),
        }
    }

    pub fn byte_size(&self) -> usize {
        SEC_INTERNAL_HEADER_SIZE
            + (self.children.len() * 4)
            + self.keys.iter().map(|k| 2 + k.bytes.len()).sum::<usize>()
    }

    pub fn is_full(&self) -> bool {
        self.byte_size() >= SEC_NODE_MAX_PAYLOAD_BYTES || self.keys.len() >= 200
    }

    pub fn find_child(&self, key: &IndexKey) -> PageId {
        match self.keys.binary_search(key) {
            Ok(idx) => self.children[idx + 1],
            Err(idx) => self.children[idx],
        }
    }

    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        let payload = page.payload_mut();
        payload[0..2].copy_from_slice(&(self.keys.len() as u16).to_le_bytes());
        payload[2..4].copy_from_slice(&(self.children.len() as u16).to_le_bytes());
        payload[4..8].fill(0); // reserved

        let mut offset = SEC_INTERNAL_HEADER_SIZE;
        for child in &self.children {
            if offset + 4 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "secondary internal page {} overflowed payload capacity",
                    page.id()
                )));
            }
            payload[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
            offset += 4;
        }

        for key in &self.keys {
            let klen = key.bytes.len();
            if offset + 2 + klen > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "secondary internal page {} overflowed payload capacity",
                    page.id()
                )));
            }
            payload[offset..offset + 2].copy_from_slice(&(klen as u16).to_le_bytes());
            offset += 2;
            payload[offset..offset + klen].copy_from_slice(&key.bytes);
            offset += klen;
        }

        if offset < PAGE_PAYLOAD_SIZE {
            payload[offset..PAGE_PAYLOAD_SIZE].fill(0);
        }

        Ok(())
    }

    pub fn deserialize(page: &Page) -> Result<Self> {
        let payload = page.payload();
        let key_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let child_count = u16::from_le_bytes([payload[2], payload[3]]) as usize;

        let mut offset = SEC_INTERNAL_HEADER_SIZE;
        let mut children = Vec::with_capacity(child_count);
        for _ in 0..child_count {
            if offset + 4 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "unexpected end of payload on secondary internal page {}",
                    page.id()
                )));
            }
            let child = u32::from_le_bytes([
                payload[offset],
                payload[offset + 1],
                payload[offset + 2],
                payload[offset + 3],
            ]);
            offset += 4;
            children.push(child);
        }

        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            if offset + 2 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "unexpected end of payload reading key on secondary internal page {}",
                    page.id()
                )));
            }
            let klen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2;

            if offset + klen > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "unexpected end of payload reading key bytes on secondary internal page {}",
                    page.id()
                )));
            }
            let key_bytes = payload[offset..offset + klen].to_vec();
            offset += klen;
            keys.push(IndexKey::new(key_bytes));
        }

        Ok(Self { keys, children })
    }

    pub fn split(&mut self) -> (SecondaryInternalNode, IndexKey) {
        let mid = self.keys.len() / 2;
        let split_key = self.keys[mid].clone();

        let right_keys = self.keys.split_off(mid + 1);
        let right_children = self.children.split_off(mid + 1);

        self.keys.pop(); // Remove promoted split_key

        let right_internal = SecondaryInternalNode {
            keys: right_keys,
            children: right_children,
        };

        (right_internal, split_key)
    }

    pub fn insert_child(&mut self, key: IndexKey, child_page_id: PageId) {
        let idx = match self.keys.binary_search(&key) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        self.keys.insert(idx, key);
        self.children.insert(idx + 1, child_page_id);
    }
}

// ---------------------------------------------------------------------------
// Secondary B+ Tree Index Handle
// ---------------------------------------------------------------------------

/// Persistent secondary B+ Tree index mapping `(IndexKey, RecordId)` tuples to physical record locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecondaryIndex {
    root_page_id: PageId,
}

impl SecondaryIndex {
    /// Creates a handle referencing an existing secondary index root page.
    pub const fn new(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    /// Creates a handle from a known root page ID.
    pub const fn from_root(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    /// Creates an empty (uninitialized) secondary index handle.
    pub const fn new_empty() -> Self {
        Self {
            root_page_id: INVALID_PAGE_ID,
        }
    }

    /// Returns the root page ID of this secondary index.
    pub const fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    /// Returns `true` if this index has no root page.
    pub const fn is_empty(&self) -> bool {
        self.root_page_id == INVALID_PAGE_ID
    }

    /// Performs a point lookup for all `RecordId`s associated with `key`.
    pub fn lookup(&self, buffer_pool: &mut BufferPool, key: &IndexKey) -> Result<Vec<RecordId>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }

        let mut curr_pid = self.root_page_id;
        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    let leaf = match SecondaryLeafNode::deserialize(page) {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_pid, false);
                            return Err(e);
                        }
                    };
                    buffer_pool.unpin_page(curr_pid, false)?;

                    let mut rids = Vec::new();
                    let mut leaf_node = leaf;

                    'scan: loop {
                        for (k, rid) in &leaf_node.entries {
                            if k == key {
                                rids.push(*rid);
                            } else if k > key {
                                break 'scan;
                            }
                        }

                        if leaf_node.next_leaf == INVALID_PAGE_ID {
                            break;
                        }

                        let leaf_pid = leaf_node.next_leaf;
                        let next_page = buffer_pool.fetch_page(leaf_pid)?;
                        leaf_node = match SecondaryLeafNode::deserialize(next_page) {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = buffer_pool.unpin_page(leaf_pid, false);
                                return Err(e);
                            }
                        };
                        buffer_pool.unpin_page(leaf_pid, false)?;
                    }

                    return Ok(rids);
                }
                PageType::Internal => {
                    let internal = match SecondaryInternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_pid, false);
                            return Err(e);
                        }
                    };
                    let next_pid = internal.find_child(key);
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary index lookup"
                    )));
                }
            }
        }
    }

    /// Performs an ordered range scan over `(IndexKey, RecordId)` entries within `[start, end)`.
    pub fn scan(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
    ) -> Result<Vec<(IndexKey, RecordId)>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }

        // Descend to leftmost leaf or leaf containing start key
        let mut curr_pid = self.root_page_id;
        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    break;
                }
                PageType::Internal => {
                    let internal = match SecondaryInternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_pid, false);
                            return Err(e);
                        }
                    };
                    let next_pid = match start {
                        Some(k) => internal.find_child(k),
                        None => internal.children[0],
                    };
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary index range scan"
                    )));
                }
            }
        }

        let mut results = Vec::new();
        let mut leaf_pid = curr_pid;

        'leaf_loop: loop {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let leaf = match SecondaryLeafNode::deserialize(page) {
                Ok(l) => l,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(leaf_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(leaf_pid, false)?;

            for (k, rid) in leaf.entries {
                if let Some(s) = start {
                    if &k < s {
                        continue;
                    }
                }
                if let Some(e) = end {
                    if &k >= e {
                        break 'leaf_loop;
                    }
                }
                results.push((k, rid));
            }

            if leaf.next_leaf == INVALID_PAGE_ID {
                break;
            }
            leaf_pid = leaf.next_leaf;
        }

        Ok(results)
    }

    /// Inserts a `(IndexKey, RecordId)` pair into the secondary index.
    pub fn insert(
        &mut self,
        buffer_pool: &mut BufferPool,
        key: &IndexKey,
        record_id: RecordId,
    ) -> Result<()> {
        if self.is_empty() {
            let root_pid = buffer_pool.allocate_page(PageType::Leaf)?;
            let mut leaf = SecondaryLeafNode::new();
            leaf.insert(key.clone(), record_id)?;
            {
                let page_mut = buffer_pool.get_page_mut(root_pid)?;
                leaf.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(root_pid, true)?;
            self.root_page_id = root_pid;
            return Ok(());
        }

        // Descend to target leaf node, tracking traversal path
        let mut path: Vec<PageId> = Vec::new();
        let mut curr_pid = self.root_page_id;

        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    break;
                }
                PageType::Internal => {
                    let internal = match SecondaryInternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_pid, false);
                            return Err(e);
                        }
                    };
                    path.push(curr_pid);
                    let next_pid = internal.find_child(key);
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary index insertion"
                    )));
                }
            }
        }

        let target_leaf_pid = curr_pid;
        let mut leaf = {
            let page = buffer_pool.fetch_page(target_leaf_pid)?;
            let l = SecondaryLeafNode::deserialize(page)?;
            buffer_pool.unpin_page(target_leaf_pid, false)?;
            l
        };

        leaf.insert(key.clone(), record_id)?;

        if !leaf.is_full() {
            let page_mut = buffer_pool.fetch_page_mut(target_leaf_pid)?;
            leaf.serialize(page_mut)?;
            buffer_pool.unpin_page(target_leaf_pid, true)?;
            return Ok(());
        }

        // Split leaf node
        let new_leaf_pid = buffer_pool.allocate_page(PageType::Leaf)?;
        let (right_leaf, split_key) = leaf.split(new_leaf_pid);

        {
            let left_page_mut = buffer_pool.fetch_page_mut(target_leaf_pid)?;
            leaf.serialize(left_page_mut)?;
            buffer_pool.unpin_page(target_leaf_pid, true)?;
        }

        {
            let right_page_mut = buffer_pool.get_page_mut(new_leaf_pid)?;
            right_leaf.serialize(right_page_mut)?;
            buffer_pool.unpin_page(new_leaf_pid, true)?;
        }

        // Propagate promoted split_key upward
        self.insert_into_parent(buffer_pool, &path, target_leaf_pid, split_key, new_leaf_pid)
    }

    fn insert_into_parent(
        &mut self,
        buffer_pool: &mut BufferPool,
        path: &[PageId],
        left_child: PageId,
        split_key: IndexKey,
        right_child: PageId,
    ) -> Result<()> {
        if path.is_empty() {
            // Root was split -> Allocate new internal root
            let new_root_pid = buffer_pool.allocate_page(PageType::Internal)?;
            let mut new_root = SecondaryInternalNode::new();
            new_root.keys.push(split_key);
            new_root.children.push(left_child);
            new_root.children.push(right_child);

            {
                let page_mut = buffer_pool.get_page_mut(new_root_pid)?;
                new_root.serialize(page_mut)?;
            }
            buffer_pool.unpin_page(new_root_pid, true)?;
            self.root_page_id = new_root_pid;
            return Ok(());
        }

        let parent_pid = path[path.len() - 1];
        let mut parent = {
            let page = buffer_pool.fetch_page(parent_pid)?;
            let node = SecondaryInternalNode::deserialize(page)?;
            buffer_pool.unpin_page(parent_pid, false)?;
            node
        };

        parent.insert_child(split_key, right_child);

        if !parent.is_full() {
            let page_mut = buffer_pool.fetch_page_mut(parent_pid)?;
            parent.serialize(page_mut)?;
            buffer_pool.unpin_page(parent_pid, true)?;
            return Ok(());
        }

        // Internal parent node also split
        let new_internal_pid = buffer_pool.allocate_page(PageType::Internal)?;
        let (right_internal, parent_split_key) = parent.split();

        {
            let page_mut = buffer_pool.fetch_page_mut(parent_pid)?;
            parent.serialize(page_mut)?;
            buffer_pool.unpin_page(parent_pid, true)?;
        }

        {
            let page_mut = buffer_pool.get_page_mut(new_internal_pid)?;
            right_internal.serialize(page_mut)?;
            buffer_pool.unpin_page(new_internal_pid, true)?;
        }

        self.insert_into_parent(
            buffer_pool,
            &path[..path.len() - 1],
            parent_pid,
            parent_split_key,
            new_internal_pid,
        )
    }

    /// Deletes a specific `(IndexKey, RecordId)` pair from the secondary index.
    pub fn delete(
        &mut self,
        buffer_pool: &mut BufferPool,
        key: &IndexKey,
        record_id: RecordId,
    ) -> Result<bool> {
        if self.is_empty() {
            return Ok(false);
        }

        let mut curr_pid = self.root_page_id;
        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    break;
                }
                PageType::Internal => {
                    let internal = match SecondaryInternalNode::deserialize(page) {
                        Ok(n) => n,
                        Err(e) => {
                            let _ = buffer_pool.unpin_page(curr_pid, false);
                            return Err(e);
                        }
                    };
                    let next_pid = internal.find_child(key);
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary index delete"
                    )));
                }
            }
        }

        let leaf_pid = curr_pid;
        let mut leaf = {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let l = SecondaryLeafNode::deserialize(page)?;
            buffer_pool.unpin_page(leaf_pid, false)?;
            l
        };

        let removed = leaf.remove(key, record_id);
        if removed {
            let page_mut = buffer_pool.fetch_page_mut(leaf_pid)?;
            leaf.serialize(page_mut)?;
            buffer_pool.unpin_page(leaf_pid, true)?;
        }

        Ok(removed)
    }

    /// Collects all physical `PageId`s currently allocated to this secondary index.
    pub fn collect_all_pages(&self, buffer_pool: &mut BufferPool) -> Result<Vec<PageId>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }

        let mut pages = Vec::new();
        let mut queue = vec![self.root_page_id];
        let mut visited = HashSet::new();

        while let Some(pid) = queue.pop() {
            if !visited.insert(pid) {
                continue;
            }
            pages.push(pid);

            let page = buffer_pool.fetch_page(pid)?;
            let ptype = page.page_type();
            match ptype {
                PageType::Internal => {
                    if let Ok(internal) = SecondaryInternalNode::deserialize(page) {
                        for child in internal.children {
                            if child != INVALID_PAGE_ID {
                                queue.push(child);
                            }
                        }
                    }
                }
                PageType::Leaf => {}
                _ => {}
            }
            buffer_pool.unpin_page(pid, false)?;
        }

        Ok(pages)
    }

    /// Validates structural invariants, key sorting, child routing, and uniqueness constraints.
    pub fn validate_tree(&self, buffer_pool: &mut BufferPool, unique: bool) -> Result<()> {
        if self.is_empty() {
            return Ok(());
        }

        let mut visited = HashSet::new();
        let mut queue = vec![(self.root_page_id, 0usize)];
        let mut leaf_pages = Vec::new();

        while let Some((pid, depth)) = queue.pop() {
            if !visited.insert(pid) {
                return Err(Error::IndexCorruption(format!(
                    "cycle detected in secondary index tree at page {pid}"
                )));
            }

            let page = buffer_pool.fetch_page(pid)?;
            let ptype = page.page_type();

            match ptype {
                PageType::Internal => {
                    let internal = SecondaryInternalNode::deserialize(page)?;
                    buffer_pool.unpin_page(pid, false)?;

                    if internal.children.len() != internal.keys.len() + 1 {
                        return Err(Error::IndexCorruption(format!(
                            "secondary internal page {pid} has {} keys but {} children",
                            internal.keys.len(),
                            internal.children.len()
                        )));
                    }

                    for i in 1..internal.keys.len() {
                        if internal.keys[i - 1] >= internal.keys[i] {
                            return Err(Error::IndexCorruption(format!(
                                "secondary internal page {pid} keys not strictly sorted"
                            )));
                        }
                    }

                    for &child in &internal.children {
                        if child == INVALID_PAGE_ID {
                            return Err(Error::IndexCorruption(format!(
                                "secondary internal page {pid} has invalid child pointer"
                            )));
                        }
                        queue.push((child, depth + 1));
                    }
                }
                PageType::Leaf => {
                    let leaf = SecondaryLeafNode::deserialize(page)?;
                    buffer_pool.unpin_page(pid, false)?;

                    for i in 1..leaf.entries.len() {
                        let prev = &leaf.entries[i - 1];
                        let curr = &leaf.entries[i];
                        if prev >= curr {
                            return Err(Error::IndexCorruption(format!(
                                "secondary leaf page {pid} entries not strictly sorted"
                            )));
                        }
                        if unique && prev.0 == curr.0 {
                            return Err(Error::IndexCorruption(format!(
                                "duplicate key in unique secondary index on leaf page {pid}"
                            )));
                        }
                    }

                    leaf_pages.push((pid, leaf.next_leaf));
                }
                other => {
                    let _ = buffer_pool.unpin_page(pid, false);
                    return Err(Error::IndexCorruption(format!(
                        "unexpected page type {other:?} at page {pid} in secondary index"
                    )));
                }
            }
        }

        Ok(())
    }
}
