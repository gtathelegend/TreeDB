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
|                Buffer Pool Manager (Planned)                |
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

* **No Layer Skipping:** Higher-level structures (such as future B+ Trees) must never perform raw file operations or compute physical byte offsets. All page reads and allocations pass through the lower storage management layers.
* **Bounded Page Size:** All structures stored inside a page must strictly fit within `PAGE_SIZE` (4096 bytes).
* **Deterministic Encoding:** Multi-byte integers are serialized exclusively in little-endian format.
