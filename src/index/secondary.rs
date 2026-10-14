use crate::engine::types::{DataType, Value};
use crate::error::{Error, Result};
use crate::index::composite::CompositeIndexKey;
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{Page, PageId, PageType, PAGE_PAYLOAD_SIZE};
use std::collections::HashSet;

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

    /// Consumes the key and returns its underlying byte vector.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Decodes an `IndexKey` byte slice back into a typed `Value` given the column's `DataType`.
    pub fn decode_bytes(bytes: &[u8], data_type: DataType) -> Result<Value> {
        if bytes.is_empty() {
            return Err(Error::IndexCorruption(
                "empty index key byte payload".to_string(),
            ));
        }

        let tag = bytes[0];
        if tag == 0 {
            return Ok(Value::Null);
        }

        match (tag, data_type) {
            (1, DataType::Boolean) => Ok(Value::Boolean(false)),
            (2, DataType::Boolean) => Ok(Value::Boolean(true)),
            (3, DataType::Int64) => {
                if bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid Int64 index key length {}",
                        bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[1..9]);
                let sortable = u64::from_be_bytes(buf);
                let original = (sortable ^ (1u64 << 63)) as i64;
                Ok(Value::Int64(original))
            }
            (4, DataType::UInt64) => {
                if bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid UInt64 index key length {}",
                        bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[1..9]);
                let u = u64::from_be_bytes(buf);
                Ok(Value::UInt64(u))
            }
            (5, DataType::Float64) => {
                if bytes.len() != 9 {
                    return Err(Error::IndexCorruption(format!(
                        "invalid Float64 index key length {}",
                        bytes.len()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[1..9]);
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
                let s = std::str::from_utf8(&bytes[1..]).map_err(|_| {
                    Error::IndexCorruption("invalid UTF-8 in String index key".to_string())
                })?;
                Ok(Value::String(s.to_string()))
            }
            (7, DataType::Bytes) => Ok(Value::Bytes(bytes[1..].to_vec())),
            _ => Err(Error::IndexCorruption(format!(
                "index key tag {tag} does not match expected data type {data_type:?}"
            ))),
        }
    }

    /// Decodes this `IndexKey` back into a typed `Value` given the column's `DataType`.
    pub fn to_value(&self, data_type: DataType) -> Result<Value> {
        Self::decode_bytes(&self.bytes, data_type)
    }
}

// ---------------------------------------------------------------------------
// Secondary B+ Tree Leaf Node
// ---------------------------------------------------------------------------

pub const SEC_LEAF_HEADER_SIZE: usize = 8;
pub const SEC_NODE_MAX_PAYLOAD_BYTES: usize = 3800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryLeafEntry {
    /// Canonical, order-preserving search key.
    pub key: IndexKey,
    /// Optional serialized covered payload bytes.
    pub payload: Vec<u8>,
    /// Physical base table record locator.
    pub rid: RecordId,
}

impl PartialOrd for SecondaryLeafEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SecondaryLeafEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.rid.cmp(&other.rid))
    }
}

impl SecondaryLeafEntry {
    /// Creates a standard entry without covered payload.
    pub fn new(key: IndexKey, rid: RecordId) -> Self {
        Self {
            key,
            payload: Vec::new(),
            rid,
        }
    }

    /// Creates an entry with covered payload.
    pub fn with_payload(key: IndexKey, payload: Vec<u8>, rid: RecordId) -> Self {
        Self { key, payload, rid }
    }
}

/// Leaf node in a secondary B+ Tree index storing sorted `SecondaryLeafEntry` items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryLeafNode {
    pub next_leaf: PageId,
    pub entries: Vec<SecondaryLeafEntry>,
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
                .map(|e| 2 + e.key.bytes.len() + 2 + e.payload.len() + 8)
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
        payload[6..8].copy_from_slice(&1u16.to_le_bytes()); // format version 1 (covering payload support)

        let mut offset = SEC_LEAF_HEADER_SIZE;
        for entry in &self.entries {
            let klen = entry.key.bytes.len();
            let plen = entry.payload.len();
            if offset + 2 + klen + 2 + plen + 8 > PAGE_PAYLOAD_SIZE {
                return Err(Error::IndexCorruption(format!(
                    "secondary leaf page {} overflowed payload capacity",
                    page.id()
                )));
            }
            payload[offset..offset + 2].copy_from_slice(&(klen as u16).to_le_bytes());
            offset += 2;
            payload[offset..offset + klen].copy_from_slice(&entry.key.bytes);
            offset += klen;
            payload[offset..offset + 2].copy_from_slice(&(plen as u16).to_le_bytes());
            offset += 2;
            payload[offset..offset + plen].copy_from_slice(&entry.payload);
            offset += plen;
            payload[offset..offset + 8].copy_from_slice(&entry.rid.to_bytes());
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
        let format_version = u16::from_le_bytes([payload[6], payload[7]]);

        let mut entries = Vec::with_capacity(entry_count);
        let mut offset = SEC_LEAF_HEADER_SIZE;

        if format_version == 0 {
            // Legacy format without payload
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

                entries.push(SecondaryLeafEntry::new(IndexKey::new(key_bytes), rid));
            }
        } else {
            // Covering format with payload
            for _ in 0..entry_count {
                if offset + 2 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload on secondary leaf page {}",
                        page.id()
                    )));
                }
                let klen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;

                if offset + klen + 2 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload reading key on secondary leaf page {}",
                        page.id()
                    )));
                }
                let key_bytes = payload[offset..offset + klen].to_vec();
                offset += klen;

                let plen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;

                if offset + plen + 8 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload reading payload on secondary leaf page {}",
                        page.id()
                    )));
                }
                let entry_payload = payload[offset..offset + plen].to_vec();
                offset += plen;

                let mut rid_bytes = [0u8; 8];
                rid_bytes.copy_from_slice(&payload[offset..offset + 8]);
                offset += 8;
                let rid = RecordId::from_bytes(rid_bytes);

                entries.push(SecondaryLeafEntry::with_payload(
                    IndexKey::new(key_bytes),
                    entry_payload,
                    rid,
                ));
            }
        }

        Ok(Self { next_leaf, entries })
    }

    pub fn insert_with_payload(
        &mut self,
        key: IndexKey,
        payload: Vec<u8>,
        rid: RecordId,
    ) -> Result<()> {
        match self
            .entries
            .binary_search_by(|e| e.key.cmp(&key).then_with(|| e.rid.cmp(&rid)))
        {
            Ok(idx) => {
                self.entries[idx].payload = payload;
                Ok(())
            }
            Err(idx) => {
                self.entries
                    .insert(idx, SecondaryLeafEntry::with_payload(key, payload, rid));
                Ok(())
            }
        }
    }

    pub fn insert(&mut self, key: IndexKey, rid: RecordId) -> Result<()> {
        self.insert_with_payload(key, Vec::new(), rid)
    }

    pub fn remove(&mut self, key: &IndexKey, rid: RecordId) -> bool {
        if let Ok(idx) = self
            .entries
            .binary_search_by(|e| e.key.cmp(key).then_with(|| e.rid.cmp(&rid)))
        {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn split(&mut self, new_page_id: PageId) -> (SecondaryLeafNode, IndexKey) {
        let mid = self.entries.len() / 2;
        let right_entries = self.entries.split_off(mid);
        let split_key = right_entries[0].key.clone();

        let right_leaf = SecondaryLeafNode {
            next_leaf: self.next_leaf,
            entries: right_entries,
        };
        self.next_leaf = new_page_id;

        (right_leaf, split_key)
    }

    /// Iterates over raw leaf entries directly from page payload without allocating Entry/Key/payload objects.
    /// Passes `(key_bytes, payload_bytes, rid)` to callback. If callback returns `false`, terminates iteration.
    /// Returns `(next_leaf, early_stopped)`.
    pub fn for_each_entry_raw<F>(page: &Page, mut f: F) -> Result<(PageId, bool)>
    where
        F: FnMut(&[u8], &[u8], RecordId) -> bool,
    {
        let payload = page.payload();
        if payload.len() < SEC_LEAF_HEADER_SIZE {
            return Err(Error::IndexCorruption(format!(
                "secondary leaf page {} payload too short",
                page.id()
            )));
        }
        let entry_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let next_leaf = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
        let format_version = u16::from_le_bytes([payload[6], payload[7]]);

        let mut offset = SEC_LEAF_HEADER_SIZE;
        if format_version == 0 {
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
                let key_bytes = &payload[offset..offset + klen];
                offset += klen;
                let mut rid_bytes = [0u8; 8];
                rid_bytes.copy_from_slice(&payload[offset..offset + 8]);
                offset += 8;
                let rid = RecordId::from_bytes(rid_bytes);

                if !f(key_bytes, &[], rid) {
                    return Ok((next_leaf, true));
                }
            }
        } else {
            for _ in 0..entry_count {
                if offset + 2 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload on secondary leaf page {}",
                        page.id()
                    )));
                }
                let klen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if offset + klen + 2 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload reading key on secondary leaf page {}",
                        page.id()
                    )));
                }
                let key_bytes = &payload[offset..offset + klen];
                offset += klen;

                let plen = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if offset + plen + 8 > PAGE_PAYLOAD_SIZE {
                    return Err(Error::IndexCorruption(format!(
                        "unexpected end of payload reading payload on secondary leaf page {}",
                        page.id()
                    )));
                }
                let entry_payload = &payload[offset..offset + plen];
                offset += plen;

                let mut rid_bytes = [0u8; 8];
                rid_bytes.copy_from_slice(&payload[offset..offset + 8]);
                offset += 8;
                let rid = RecordId::from_bytes(rid_bytes);

                if !f(key_bytes, entry_payload, rid) {
                    return Ok((next_leaf, true));
                }
            }
        }

        Ok((next_leaf, false))
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
            Ok(idx) => {
                let last_idx = idx + self.keys[idx..].iter().take_while(|k| *k == key).count() - 1;
                self.children[last_idx + 1]
            }
            Err(idx) => self.children[idx],
        }
    }

    /// Finds the leftmost child page ID that could contain `key` (for point lookups and range scans).
    pub fn find_leftmost_child(&self, key: &IndexKey) -> PageId {
        match self.keys.binary_search(key) {
            Ok(idx) => {
                let first_idx = self.keys[..=idx]
                    .iter()
                    .rposition(|k| k != key)
                    .map(|p| p + 1)
                    .unwrap_or(0);
                self.children[first_idx]
            }
            Err(idx) => self.children[idx],
        }
    }

    /// Zero-heap-allocation child routing directly on the raw page payload using key bytes.
    /// If `leftmost` is true, routes to the leftmost child that could contain `target_bytes` (for lookups and scans).
    /// If `leftmost` is false, routes to the right child (for insertions).
    pub fn find_child_raw_bytes(
        page: &Page,
        target_bytes: &[u8],
        leftmost: bool,
    ) -> Result<PageId> {
        let payload = page.payload();
        if payload.len() < SEC_INTERNAL_HEADER_SIZE {
            return Err(Error::IndexCorruption(format!(
                "secondary internal page {} payload too short",
                page.id()
            )));
        }
        let key_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let child_count = u16::from_le_bytes([payload[2], payload[3]]) as usize;
        if child_count == 0 {
            return Err(Error::IndexCorruption(format!(
                "secondary internal page {} has zero children",
                page.id()
            )));
        }

        let mut offset = SEC_INTERNAL_HEADER_SIZE;
        let children_start = offset;
        let children_bytes = child_count * 4;
        if offset + children_bytes > PAGE_PAYLOAD_SIZE {
            return Err(Error::IndexCorruption(format!(
                "unexpected end of payload reading children on secondary internal page {}",
                page.id()
            )));
        }
        offset += children_bytes;

        let get_child = |idx: usize| -> Result<PageId> {
            if idx >= child_count {
                return Err(Error::IndexCorruption(format!(
                    "child index {idx} out of range {child_count} on secondary internal page {}",
                    page.id()
                )));
            }
            let child_off = children_start + idx * 4;
            Ok(u32::from_le_bytes([
                payload[child_off],
                payload[child_off + 1],
                payload[child_off + 2],
                payload[child_off + 3],
            ]))
        };

        if leftmost {
            for i in 0..key_count {
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
                let kbytes = &payload[offset..offset + klen];
                offset += klen;

                if target_bytes <= kbytes {
                    return get_child(i);
                }
            }
            get_child(key_count)
        } else {
            for i in 0..key_count {
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
                let kbytes = &payload[offset..offset + klen];
                offset += klen;

                if target_bytes < kbytes {
                    return get_child(i);
                }
            }
            get_child(key_count)
        }
    }

    /// Zero-heap-allocation child routing directly on the raw page payload.
    /// If `leftmost` is true, routes to the leftmost child that could contain `key` (for lookups and scans).
    /// If `leftmost` is false, routes to the right child (for insertions).
    pub fn find_child_raw(page: &Page, key: &IndexKey, leftmost: bool) -> Result<PageId> {
        Self::find_child_raw_bytes(page, key.as_bytes(), leftmost)
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

    /// Performs a point lookup for all entries matching `key`, returning `SecondaryLeafEntry`s.
    pub fn lookup_entries(
        &self,
        buffer_pool: &mut BufferPool,
        key: &IndexKey,
    ) -> Result<Vec<SecondaryLeafEntry>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }

        let mut curr_pid = self.root_page_id;
        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();

            match page_type {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    let mut results = Vec::new();
                    let target_bytes = key.as_bytes();
                    let mut leaf_pid = curr_pid;

                    loop {
                        let leaf_page = buffer_pool.fetch_page(leaf_pid)?;
                        let (next_leaf, stopped) =
                            SecondaryLeafNode::for_each_entry_raw(leaf_page, |kbytes, pl, rid| {
                                if kbytes == target_bytes {
                                    results.push(SecondaryLeafEntry::with_payload(
                                        key.clone(),
                                        pl.to_vec(),
                                        rid,
                                    ));
                                }
                                kbytes <= target_bytes
                            })?;
                        buffer_pool.unpin_page(leaf_pid, false)?;

                        if stopped || next_leaf == INVALID_PAGE_ID {
                            break;
                        }
                        leaf_pid = next_leaf;
                    }

                    return Ok(results);
                }
                PageType::Internal => {
                    let next_pid = SecondaryInternalNode::find_child_raw(page, key, true)?;
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

    /// Performs a point lookup for all `RecordId`s associated with `key` with zero entry deserialization.
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
                    buffer_pool.unpin_page(curr_pid, false)?;
                    let mut results = Vec::new();
                    let target_bytes = key.as_bytes();
                    let mut leaf_pid = curr_pid;

                    loop {
                        let leaf_page = buffer_pool.fetch_page(leaf_pid)?;
                        let (next_leaf, stopped) = SecondaryLeafNode::for_each_entry_raw(
                            leaf_page,
                            |kbytes, _pl, rid| {
                                if kbytes == target_bytes {
                                    results.push(rid);
                                }
                                kbytes <= target_bytes
                            },
                        )?;
                        buffer_pool.unpin_page(leaf_pid, false)?;

                        if stopped || next_leaf == INVALID_PAGE_ID {
                            break;
                        }
                        leaf_pid = next_leaf;
                    }

                    return Ok(results);
                }
                PageType::Internal => {
                    let next_pid = SecondaryInternalNode::find_child_raw(page, key, true)?;
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

    /// Performs an ordered range scan returning `RecordId`s within `[start, end)` directly with zero key allocation.
    pub fn scan_rids(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
    ) -> Result<Vec<RecordId>> {
        self.scan_rids_with_limit(buffer_pool, start, end, None)
    }

    /// Performs an ordered range scan returning up to `limit` `RecordId`s within `[start, end)`.
    pub fn scan_rids_with_limit(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
        limit: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        if self.is_empty() || limit == Some(0) {
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
                    let next_pid = match start {
                        Some(k) => SecondaryInternalNode::find_child_raw(page, k, true)?,
                        None => {
                            let payload = page.payload();
                            if payload.len() < SEC_INTERNAL_HEADER_SIZE + 4 {
                                buffer_pool.unpin_page(curr_pid, false)?;
                                return Err(Error::IndexCorruption(format!(
                                    "secondary internal page {curr_pid} corrupted header"
                                )));
                            }
                            u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]])
                        }
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
        let start_bytes = start.map(|k| k.as_bytes());
        let end_bytes = end.map(|k| k.as_bytes());

        while leaf_pid != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let (next_leaf, stopped) =
                SecondaryLeafNode::for_each_entry_raw(page, |kbytes, _pl, rid| {
                    if let Some(s) = start_bytes {
                        if kbytes < s {
                            return true;
                        }
                    }
                    if let Some(e) = end_bytes {
                        if kbytes >= e {
                            return false;
                        }
                    }
                    results.push(rid);
                    if let Some(lim) = limit {
                        if results.len() >= lim {
                            return false;
                        }
                    }
                    true
                })?;
            buffer_pool.unpin_page(leaf_pid, false)?;

            if stopped || limit.is_some_and(|lim| results.len() >= lim) {
                break;
            }
            leaf_pid = next_leaf;
        }

        Ok(results)
    }

    /// Performs an ordered reverse range scan returning `RecordId`s in descending order within `[start, end)`.
    pub fn scan_rids_reverse(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
        limit: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        let entries = self.scan_entries_reverse(buffer_pool, start, end, limit)?;
        Ok(entries.into_iter().map(|e| e.rid).collect())
    }

    /// Performs an ordered range scan returning `SecondaryLeafEntry`s within `[start, end)`.
    pub fn scan_entries(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
    ) -> Result<Vec<SecondaryLeafEntry>> {
        self.scan_entries_with_limit(buffer_pool, start, end, None)
    }

    /// Performs an ordered range scan returning up to `limit` `SecondaryLeafEntry`s within `[start, end)`.
    pub fn scan_entries_with_limit(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
        limit: Option<usize>,
    ) -> Result<Vec<SecondaryLeafEntry>> {
        if self.is_empty() || limit == Some(0) {
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
                    let next_pid = match start {
                        Some(k) => SecondaryInternalNode::find_child_raw(page, k, true)?,
                        None => {
                            let payload = page.payload();
                            if payload.len() < SEC_INTERNAL_HEADER_SIZE + 4 {
                                buffer_pool.unpin_page(curr_pid, false)?;
                                return Err(Error::IndexCorruption(format!(
                                    "secondary internal page {curr_pid} corrupted header"
                                )));
                            }
                            u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]])
                        }
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
        let start_bytes = start.map(|k| k.as_bytes());
        let end_bytes = end.map(|k| k.as_bytes());

        while leaf_pid != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let (next_leaf, stopped) =
                SecondaryLeafNode::for_each_entry_raw(page, |kbytes, pl, rid| {
                    if let Some(s) = start_bytes {
                        if kbytes < s {
                            return true;
                        }
                    }
                    if let Some(e) = end_bytes {
                        if kbytes >= e {
                            return false;
                        }
                    }
                    results.push(SecondaryLeafEntry::with_payload(
                        IndexKey::new(kbytes.to_vec()),
                        pl.to_vec(),
                        rid,
                    ));
                    if let Some(lim) = limit {
                        if results.len() >= lim {
                            return false;
                        }
                    }
                    true
                })?;
            buffer_pool.unpin_page(leaf_pid, false)?;

            if stopped || limit.is_some_and(|lim| results.len() >= lim) {
                break;
            }
            leaf_pid = next_leaf;
        }

        Ok(results)
    }

    /// Performs an in-place ordered scan over `[start, end)` invoking a callback for each entry
    /// without heap-allocating `SecondaryLeafEntry` or copying key/payload bytes.
    pub fn scan_raw_entries<F>(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
        mut visitor: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8], &[u8], RecordId) -> Result<bool>,
    {
        if self.is_empty() {
            return Ok(());
        }
        if let (Some(s), Some(e)) = (start, end) {
            if s >= e {
                return Ok(());
            }
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
                    let next_pid = match start {
                        Some(k) => SecondaryInternalNode::find_child_raw(page, k, true)?,
                        None => {
                            let payload = page.payload();
                            if payload.len() < SEC_INTERNAL_HEADER_SIZE + 4 {
                                buffer_pool.unpin_page(curr_pid, false)?;
                                return Err(Error::IndexCorruption(format!(
                                    "secondary internal page {curr_pid} corrupted header"
                                )));
                            }
                            u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]])
                        }
                    };
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary index raw scan"
                    )));
                }
            }
        }

        let mut leaf_pid = curr_pid;
        let start_bytes = start.map(|k| k.as_bytes());
        let end_bytes = end.map(|k| k.as_bytes());
        let mut keep_going = true;
        let mut err: Option<Error> = None;

        while leaf_pid != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let (next_leaf, stopped) =
                SecondaryLeafNode::for_each_entry_raw(page, |kbytes, pl, rid| {
                    if let Some(s) = start_bytes {
                        if kbytes < s {
                            return true;
                        }
                    }
                    if let Some(e) = end_bytes {
                        if kbytes >= e {
                            return false;
                        }
                    }
                    match visitor(kbytes, pl, rid) {
                        Ok(true) => true,
                        Ok(false) => {
                            keep_going = false;
                            false
                        }
                        Err(e) => {
                            err = Some(e);
                            keep_going = false;
                            false
                        }
                    }
                })?;
            buffer_pool.unpin_page(leaf_pid, false)?;

            if let Some(e) = err {
                return Err(e);
            }
            if stopped || !keep_going {
                break;
            }
            leaf_pid = next_leaf;
        }

        Ok(())
    }

    /// Performs an ordered reverse range scan returning `SecondaryLeafEntry`s in descending order within `[start, end)`.
    pub fn scan_entries_reverse(
        &self,
        buffer_pool: &mut BufferPool,
        start: Option<&IndexKey>,
        end: Option<&IndexKey>,
        limit: Option<usize>,
    ) -> Result<Vec<SecondaryLeafEntry>> {
        if self.is_empty() || limit == Some(0) {
            return Ok(Vec::new());
        }
        if let (Some(s), Some(e)) = (start, end) {
            if s >= e {
                return Ok(Vec::new());
            }
        }

        let mut curr_pid = self.root_page_id;
        let mut stack: Vec<(PageId, usize)> = Vec::new();

        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            let page_type = page.page_type();
            match page_type {
                PageType::Leaf => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    break;
                }
                PageType::Internal => {
                    let internal_node = SecondaryInternalNode::deserialize(page)?;
                    let child_idx = match end {
                        Some(end_key) => {
                            let mut count = 0;
                            for (i, k) in internal_node.keys.iter().enumerate() {
                                if k < end_key {
                                    count = i + 1;
                                } else {
                                    break;
                                }
                            }
                            count
                        }
                        None => internal_node.children.len().saturating_sub(1),
                    };
                    let next_pid = internal_node.children[child_idx];
                    buffer_pool.unpin_page(curr_pid, false)?;
                    stack.push((curr_pid, child_idx));
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during secondary reverse scan"
                    )));
                }
            }
        }

        let mut results = Vec::new();
        let mut stop = false;

        loop {
            let page = buffer_pool.fetch_page(curr_pid)?;
            if page.page_type() != PageType::Leaf {
                buffer_pool.unpin_page(curr_pid, false)?;
                return Err(Error::TreeCorruption("expected secondary leaf page".into()));
            }
            let leaf = SecondaryLeafNode::deserialize(page)?;
            buffer_pool.unpin_page(curr_pid, false)?;

            for entry in leaf.entries.into_iter().rev() {
                if let Some(e) = end {
                    if &entry.key >= e {
                        continue;
                    }
                }
                if let Some(s) = start {
                    if &entry.key < s {
                        stop = true;
                        break;
                    }
                }
                results.push(entry);
                if let Some(lim) = limit {
                    if results.len() >= lim {
                        stop = true;
                        break;
                    }
                }
            }

            if stop {
                break;
            }

            // Step to previous leaf
            let mut next_leaf_id = INVALID_PAGE_ID;
            while let Some((parent_id, child_idx)) = stack.pop() {
                if child_idx > 0 {
                    let new_child_idx = child_idx - 1;
                    let parent_page = buffer_pool.fetch_page(parent_id)?;
                    let parent_node = SecondaryInternalNode::deserialize(parent_page)?;
                    let target_child_id = parent_node.children[new_child_idx];
                    buffer_pool.unpin_page(parent_id, false)?;
                    stack.push((parent_id, new_child_idx));

                    // Descend rightmost
                    let mut desc_id = target_child_id;
                    loop {
                        let desc_page = buffer_pool.fetch_page(desc_id)?;
                        match desc_page.page_type() {
                            PageType::Internal => {
                                let desc_node = SecondaryInternalNode::deserialize(desc_page)?;
                                let last_idx = desc_node.children.len().saturating_sub(1);
                                let last_child = desc_node.children[last_idx];
                                buffer_pool.unpin_page(desc_id, false)?;
                                stack.push((desc_id, last_idx));
                                desc_id = last_child;
                            }
                            PageType::Leaf => {
                                buffer_pool.unpin_page(desc_id, false)?;
                                next_leaf_id = desc_id;
                                break;
                            }
                            _ => {
                                buffer_pool.unpin_page(desc_id, false)?;
                                return Err(Error::TreeCorruption(
                                    "unexpected page type during secondary reverse traversal"
                                        .into(),
                                ));
                            }
                        }
                    }
                    break;
                }
            }

            if next_leaf_id == INVALID_PAGE_ID {
                break;
            }
            curr_pid = next_leaf_id;
        }

        Ok(results)
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
                    let next_pid = match start {
                        Some(k) => SecondaryInternalNode::find_child_raw(page, k, true)?,
                        None => {
                            let payload = page.payload();
                            if payload.len() < SEC_INTERNAL_HEADER_SIZE + 4 {
                                buffer_pool.unpin_page(curr_pid, false)?;
                                return Err(Error::IndexCorruption(format!(
                                    "secondary internal page {curr_pid} corrupted header"
                                )));
                            }
                            u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]])
                        }
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
        let start_bytes = start.map(|k| k.as_bytes());
        let end_bytes = end.map(|k| k.as_bytes());

        while leaf_pid != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let (next_leaf, stopped) =
                SecondaryLeafNode::for_each_entry_raw(page, |kbytes, _pl, rid| {
                    if let Some(s) = start_bytes {
                        if kbytes < s {
                            return true;
                        }
                    }
                    if let Some(e) = end_bytes {
                        if kbytes >= e {
                            return false;
                        }
                    }
                    results.push((IndexKey::new(kbytes.to_vec()), rid));
                    true
                })?;
            buffer_pool.unpin_page(leaf_pid, false)?;

            if stopped {
                break;
            }
            leaf_pid = next_leaf;
        }

        Ok(results)
    }

    /// Enumerates distinct leading-prefix values of length `skipped_prefix_len` in deterministic key order.
    pub fn distinct_prefix_values(
        &self,
        buffer_pool: &mut BufferPool,
        total_columns: usize,
        skipped_prefix_len: usize,
        col_types: &[DataType],
    ) -> Result<Vec<Vec<Value>>> {
        if self.is_empty() || skipped_prefix_len == 0 {
            return Ok(Vec::new());
        }

        // Descend to leftmost leaf page
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
                    let payload = page.payload();
                    if payload.len() < SEC_INTERNAL_HEADER_SIZE + 4 {
                        buffer_pool.unpin_page(curr_pid, false)?;
                        return Err(Error::IndexCorruption(format!(
                            "secondary internal page {curr_pid} corrupted header"
                        )));
                    }
                    let next_pid =
                        u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
                    buffer_pool.unpin_page(curr_pid, false)?;
                    curr_pid = next_pid;
                }
                other => {
                    buffer_pool.unpin_page(curr_pid, false)?;
                    return Err(Error::TreeCorruption(format!(
                        "unexpected page type {other:?} at page {curr_pid} during distinct prefix scan"
                    )));
                }
            }
        }

        let mut distinct_prefixes: Vec<Vec<Value>> = Vec::new();
        let mut last_prefix: Option<Vec<Value>> = None;
        let mut leaf_pid = curr_pid;

        while leaf_pid != INVALID_PAGE_ID {
            let page = buffer_pool.fetch_page(leaf_pid)?;
            let leaf = match SecondaryLeafNode::deserialize(page) {
                Ok(l) => l,
                Err(e) => {
                    let _ = buffer_pool.unpin_page(leaf_pid, false);
                    return Err(e);
                }
            };
            buffer_pool.unpin_page(leaf_pid, false)?;

            for entry in &leaf.entries {
                let prefix = CompositeIndexKey::decode_prefix_values(
                    &entry.key.bytes,
                    total_columns,
                    skipped_prefix_len,
                    col_types,
                )?;

                if last_prefix.as_ref() != Some(&prefix) {
                    last_prefix = Some(prefix.clone());
                    distinct_prefixes.push(prefix);
                }
            }

            leaf_pid = leaf.next_leaf;
        }

        Ok(distinct_prefixes)
    }

    /// Inserts a `(IndexKey, covered_payload, RecordId)` pair into the secondary index.
    pub fn insert_with_payload(
        &mut self,
        buffer_pool: &mut BufferPool,
        key: &IndexKey,
        payload: &[u8],
        record_id: RecordId,
    ) -> Result<()> {
        if self.is_empty() {
            let root_pid = buffer_pool.allocate_page(PageType::Leaf)?;
            let mut leaf = SecondaryLeafNode::new();
            leaf.insert_with_payload(key.clone(), payload.to_vec(), record_id)?;
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
                    let next_pid = SecondaryInternalNode::find_child_raw(page, key, false)?;
                    path.push(curr_pid);
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

        leaf.insert_with_payload(key.clone(), payload.to_vec(), record_id)?;

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

    /// Inserts a `(IndexKey, RecordId)` pair into the secondary index.
    pub fn insert(
        &mut self,
        buffer_pool: &mut BufferPool,
        key: &IndexKey,
        record_id: RecordId,
    ) -> Result<()> {
        self.insert_with_payload(buffer_pool, key, &[], record_id)
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
                    let next_pid = SecondaryInternalNode::find_child_raw(page, key, true)?;
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

        let mut curr_leaf_pid = curr_pid;
        loop {
            let mut leaf = {
                let page = buffer_pool.fetch_page(curr_leaf_pid)?;
                let l = SecondaryLeafNode::deserialize(page)?;
                buffer_pool.unpin_page(curr_leaf_pid, false)?;
                l
            };

            let removed = leaf.remove(key, record_id);
            if removed {
                let page_mut = buffer_pool.fetch_page_mut(curr_leaf_pid)?;
                leaf.serialize(page_mut)?;
                buffer_pool.unpin_page(curr_leaf_pid, true)?;
                return Ok(true);
            }

            if leaf.next_leaf == INVALID_PAGE_ID {
                return Ok(false);
            }
            if let Some(first) = leaf.entries.first() {
                if &first.key > key {
                    return Ok(false);
                }
            }
            curr_leaf_pid = leaf.next_leaf;
        }
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
                        if unique && prev.key == curr.key {
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
