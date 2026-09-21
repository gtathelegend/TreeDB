# TreeDB B+ Tree Index Engine

This document details the persistent B+ Tree node formats, routing rules, capacity calculations, and read-only traversal algorithms implemented in Phase 3.

> **Phase 3 Scope Notice:** Phase 3 implements persistent node structures and read-only point search/traversal. Mutation (insertion, splitting, deletion, merging) and range iterators are planned for Phase 4.

---

## 1. Why a B+ Tree?

* **Disk-Block Alignment:** B+ Tree nodes map 1-to-1 with fixed 4096-byte database pages (`PAGE_SIZE`), maximizing I/O efficiency.
* **High Fanout & Low Depth:** A fanout of up to 341 children per internal node keeps the tree shallow (depth 3 can route millions of keys), minimizing disk seeks.
* **Sequential Range Scans:** Leaf nodes form a singly-linked list via `next_leaf` pointers, enabling efficient ordered scans in future phases.
* **Separation of Index and Data:** Interior routing nodes contain only separator keys, leaving maximum space for child pointers.

---

## 2. Core Primitives

* **`Key`:** `u64` (8 bytes, little-endian).
* **`PageId`:** `u32` (4 bytes, little-endian).
* **`RecordId`:** `(page_id: u32, slot_id: u32)` (8 bytes total, little-endian).
* **`INVALID_PAGE_ID`:** `0` (used as the sentinel for "no next leaf" and empty root).

---

## 3. Leaf Node Format (`PageType::Leaf`)

Leaf nodes store sorted key-record pairs:

```text
+---------------------------------------------------------------+
| Standard Page Header (8 bytes): [page_id: u32 | type=3 | ...] |
+---------------------------------------------------------------+
| Leaf Header (6 bytes):                                        |
|   key_count: u16 (2B) | next_leaf: PageId (4B)                |
+---------------------------------------------------------------+
| Entries Array (up to 255 entries, 16 bytes each):             |
|   [Key (8B) | RecordId (8B)]                                  |
|   [Key (8B) | RecordId (8B)]                                  |
|   ...                                                         |
+---------------------------------------------------------------+
```

### Capacity Derivation:
* `PAGE_PAYLOAD_SIZE` = 4088 bytes
* `LEAF_HEADER_SIZE` = 6 bytes (`key_count: u16` + `next_leaf: u32`)
* `LEAF_ENTRY_SIZE` = 16 bytes (`Key: u64` + `RecordId: 8 bytes`)
* **`LEAF_MAX_ENTRIES`** = `(4088 - 6) / 16 = 255 entries`
* Space used: `6 + (255 * 16) = 4086 bytes` (2 bytes unused padding).

### Invariants:
1. Keys must be sorted in strictly ascending order: `keys[i] < keys[i+1]`.
2. No duplicate keys allowed within a leaf node.
3. `key_count <= 255`.

---

## 4. Internal Node Format (`PageType::Internal`)

Internal nodes store `n` separator keys and `n + 1` child page pointers:

```text
+---------------------------------------------------------------+
| Standard Page Header (8 bytes): [page_id: u32 | type=2 | ...] |
+---------------------------------------------------------------+
| Internal Header (6 bytes):                                    |
|   key_count: u16 (2B) | first_child: PageId (4B)              |
+---------------------------------------------------------------+
| Entries Array (up to 340 entries, 12 bytes each):             |
|   [SeparatorKey 0 (8B) | ChildPageId 1 (4B)]                  |
|   [SeparatorKey 1 (8B) | ChildPageId 2 (4B)]                  |
|   ...                                                         |
+---------------------------------------------------------------+
```

### Routing Convention:
For separators `[K0, K1, ..., Kn-1]` and child pointers `[C0, C1, ..., Cn]`:
* `search_key < K0`        => route to `C0`
* `K0 <= search_key < K1`  => route to `C1`
* `Ki <= search_key < Ki+1`=> route to `Ci+1`
* `search_key >= Kn-1`     => route to `Cn`

### Capacity Derivation:
* `PAGE_PAYLOAD_SIZE` = 4088 bytes
* `INTERNAL_HEADER_SIZE` = 6 bytes (`key_count: u16` + `first_child: u32`)
* `INTERNAL_ENTRY_SIZE` = 12 bytes (`Key: u64` + `PageId: u32`)
* **`INTERNAL_MAX_KEYS`** = `(4088 - 6) / 12 = 340 keys` (routes to 341 children).
* Space used: `6 + (340 * 12) = 4086 bytes` (2 bytes unused padding).

### Invariants:
1. `children.len() == keys.len() + 1`.
2. Separator keys must be strictly ascending: `keys[i] < keys[i+1]`.
3. `keys.len() <= 340`.
4. All child pointers must be valid non-zero `PageId`s.

---

## 5. Search Algorithm & Pinning Discipline

Point lookup (`BPlusTree::get(&self, buffer_pool, key) -> Result<Option<RecordId>>`):

```text
1. If root_page_id == INVALID_PAGE_ID (0) -> Return Ok(None) (Empty Tree)
2. curr_page_id = root_page_id
3. Loop:
   a. page = buffer_pool.fetch_page(curr_page_id)?
   b. Match page.page_type():
      - PageType::Leaf:
          leaf = LeafNode::deserialize(page)?
          buffer_pool.unpin_page(curr_page_id, false)?
          return Ok(leaf.get(key))
      - PageType::Internal:
          internal = InternalNode::deserialize(page)?
          next_page_id = internal.find_child(key)
          buffer_pool.unpin_page(curr_page_id, false)?
          curr_page_id = next_page_id
      - Other:
          buffer_pool.unpin_page(curr_page_id, false)?
          return Err(Error::TreeCorruption(...))
```

### Pinning Guarantee:
Every page fetched during traversal is strictly unpinned before descending to its child or returning. This prevents buffer frame pin leaks and keeps the Buffer Pool evictable.
