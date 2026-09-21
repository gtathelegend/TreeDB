# TreeDB Architecture

TreeDB is structured into clear, decoupled layers of abstraction. Each layer has well-defined responsibilities and invariant boundaries.

```text
+-------------------------------------------------------------+
|                      Application / CLI                      |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|                  TreeDB Engine API (Planned)                |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|              B+ Tree Index Engine (Implemented)             |
|  (Search, Insert, Delete, Rebalance, Collapse, Range Scan)  |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|                  Record Store (Planned)                     |
|           (Slotted Pages, Variable Length Tuples)           |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|              Buffer Pool Manager (Implemented)              |
|             (Page Cache, Pinning, LRU Eviction)             |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|                 Page Manager (Implemented)                  |
|     (Disk I/O, Offset Calculation, Append Allocation)       |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|                  Database File (.treedb)                    |
|             (Page 0: Header, Page 1+: Data Pages)           |
+-------------------------------------------------------------+
```

---

## 1. Index Layer (`src/index`)

### `BPlusTree`
* **Role:** Persistent B+ Tree coordinator managing index operations through the `BufferPool`.
* **Responsibilities:**
  * Point lookup (`get`) via root-to-leaf binary search traversal.
  * Dynamic insertion (`insert`) with leaf and internal splits, separator propagation, and root elevation.
  * Dynamic deletion (`delete`) with leaf/internal underflow rebalancing, sibling redistribution, merging, and root collapse.
  * Ordered sequential range scanning (`scan`) utilizing leaf `next_leaf` chains.
  * Safe, leak-free unpinning of all visited pages across every operation path.
  * Empty-tree state handling (`root_page_id == INVALID_PAGE_ID`).

### `InternalNode` & `LeafNode`
* **Role:** In-memory structured representation of interior routing and leaf data pages.
* **Responsibilities:**
  * `InternalNode`: Stores up to 340 separator keys routing to 341 children via binary search.
  * `LeafNode`: Stores up to 255 sorted `(Key, RecordId)` entries with sibling links.
  * Structural validation during deterministic serialization and deserialization.

### `RecordId`
* **Role:** Fixed-width 8-byte pointer `(page_id: u32, slot_id: u32)` referencing tuples in the storage engine.

---

## 2. Storage Layer (`src/storage`)

### `BufferPool`
* **Role:** In-memory caching engine maintaining a fixed number of page frames.
* **Responsibilities:**
  * Fast cache hit lookup via `PageId -> FrameId` page table.
  * Pin count management preventing eviction of actively referenced pages.
  * Dirty page tracking ensuring all modifications are written back before frame reuse.
  * Deterministic Least Recently Used (LRU) replacement policy.
  * Tracking hit/miss/eviction/flush statistics and hit rate.

### `PageManager`
* **Role:** Manages reading and writing fixed-size (4096-byte) blocks to and from disk.
* **Responsibilities:**
  * Opening and initializing the `.treedb` database file.
  * Validating file integrity and magic headers upon open.
  * Allocating new sequential pages (append-only allocation for Phase 1).
  * Ensuring physical disk sync via `sync()`.

### `Page` & `DbHeader`
* **Role:** Block-level in-memory representations and on-disk Page 0 metadata.

---

## 3. Layer Invariants

* **No Layer Skipping:** Higher-level structures (`BPlusTree`) interact exclusively through the `BufferPool` and never perform raw disk I/O directly.
* **Pin Leak Prevention:** Traversal routines must strictly unpin each page before descending or returning.
* **Bounded In-Memory Footprint:** The buffer pool never allocates more memory frames than its configured capacity.
* **Eviction Safety:** Pinned frames (`pin_count > 0`) are strictly protected from eviction.
* **Dirty Eviction Durability:** Dirty pages selected for eviction are flushed and verified on disk before their frame is recycled.
* **Bounded Page Size:** All structures stored inside a page must strictly fit within `PAGE_SIZE` (4096 bytes).
* **Deterministic Encoding:** Multi-byte integers are serialized exclusively in little-endian format.
