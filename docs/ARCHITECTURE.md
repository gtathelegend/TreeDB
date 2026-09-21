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
|                   B+ Tree Index (Planned)                   |
|          (Leaf / Internal Nodes, Split/Merge Logic)         |
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

## 1. Storage Layer (`src/storage`)

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

### `Page`
* **Role:** In-memory representation of a physical database page.
* **Responsibilities:**
  * Fixed byte size: 4096 bytes (`PAGE_SIZE`).
  * Header layout: Page ID (4 bytes), Page Type tag (1 byte), Flags (1 byte), Reserved (2 bytes).
  * Payload area: 4088 bytes (`PAGE_PAYLOAD_SIZE`).
  * Self-validation against page corruption and invalid type tags.

### `DbHeader`
* **Role:** Database identification and global metadata stored on Page 0.
* **Fields:**
  * Magic identifier (`TREEDB01`)
  * Format version (currently 1)
  * Page size (4096)
  * Total page count
  * Root Page ID (points to B+ Tree root page)
  * Metadata Page ID (points to Page 1 catalog)

---

## 2. Layer Invariants

* **No Layer Skipping:** Higher-level structures (such as future B+ Trees) must interact exclusively through the `BufferPool` and never perform raw disk I/O directly.
* **Bounded In-Memory Footprint:** The buffer pool never allocates more memory frames than its configured capacity.
* **Eviction Safety:** Pinned frames (`pin_count > 0`) are strictly protected from eviction.
* **Dirty Eviction Durability:** Dirty pages selected for eviction are flushed and verified on disk before their frame is recycled.
* **Bounded Page Size:** All structures stored inside a page must strictly fit within `PAGE_SIZE` (4096 bytes).
* **Deterministic Encoding:** Multi-byte integers are serialized exclusively in little-endian format.
