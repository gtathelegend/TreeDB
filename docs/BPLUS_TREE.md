# TreeDB B+ Tree Index Engine

This document details the persistent B+ Tree node formats, routing rules, capacity calculations, point search traversal, and dynamic insertion algorithms implemented in TreeDB.

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

## 5. Point Search Algorithm & Pinning Discipline

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

---

## 6. Insertion & Splitting Algorithm (`insert`)

TreeDB supports dynamic insertion with recursive parent propagation and root growth:

```text
insert(key, record_id)
        │
        ▼
   Tree Empty? ─────────► Yes ──► Allocate Leaf Root ──► Set DbHeader Root ──► Done
        │ No
        ▼
Traverse to Target Leaf (recording internal PageId path stack)
        │
        ▼
Target Leaf Full? (count == 255)
   ├─► No:  Insert key in sorted position ──► Write dirty leaf ──► Done
   └─► Yes: Split Leaf:
              - Combined entries = 256
              - Left leaf keeps 128 entries
              - Right leaf (allocated) gets 128 entries
              - next_leaf links updated (Left -> Right -> OldNext)
              - Promoted separator = right_leaf.entries[0].key
              - Propagate (sep_key, right_leaf_id) up path stack
                    │
                    ▼
     Parent Full? (keys == 340)
        ├─► No:  Insert (sep_key, right_child) in parent ──► Done
        └─► Yes: Split Internal:
                   - Combined keys = 341, children = 342
                   - mid = 170
                   - Promoted key = keys[170]
                   - Left node keeps keys[0..170], children[0..=170]
                   - Right node gets keys[171..341], children[171..342]
                   - Propagate (promoted_key, right_int_id) up
                         │
                         ▼
        Path Empty? (Root Split)
           └─► Allocate new Internal root with [promoted_key] and [old_root, right_child]
           └─► Update self.root_page_id and persist in Page 0 DbHeader
```

### Duplicate Key Policy
Attempting to insert a key that already exists returns `Err(Error::DuplicateKey(key))` and leaves the existing entry and tree structure unmodified.

### Persistence & Durability
* All modified and allocated pages are marked dirty in the `BufferPool`.
* When the tree root splits, `set_root_page_id` updates and persists Page 0 immediately.
* Invoking `buffer_pool.sync()` flushes all dirty frames to disk and calls `sync_all()`.

---

## 7. Current Limitations & Scope

* **No WAL / Crash Recovery:** Multi-page splits are not transaction-logged; crash recovery is deferred to a future phase.
* **No Deletion or Redistribution:** Key removal and node underflow merging are planned for Phase 5.
* **Single-Threaded:** Synchronization and concurrency control will be introduced after single-threaded verification.
