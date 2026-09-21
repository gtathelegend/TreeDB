# TreeDB Architecture

TreeDB is structured into clear, decoupled layers of abstraction. Each layer has well-defined responsibilities and invariant boundaries.

```text
+-------------------------------------------------------------+
|                      Application / CLI                      |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|               TreeDb Engine API (Implemented)               |
|            (put, get, delete, scan, sync, open)             |
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
|                Record Store (Implemented)                   |
|     (Slotted Pages, Variable-Length Payloads, Compaction)   |
+-------------------------------------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|              Buffer Pool Manager (Implemented)              |
|      (Page Cache, Pinning, LRU Eviction, WAL Flush Order)   |
+-------------------------------------------------------------+
                               |
                               v
+------------------------------+------------------------------+
|                                                             |
v                                                             v
+------------------------------+ +----------------------------+
|   WAL Manager (Implemented)  | | Page Manager (Implemented) |
| (Log Appending, LSN, Flush)  | |  (Disk I/O, Page Allocation)|
+------------------------------+ +----------------------------+
               |                                |
               v                                v
+------------------------------+ +----------------------------+
|     WAL File (.treedb.wal)   | |   Database File (.treedb)  |
|  (Header, LSN Log Records)   | |  (Page 0: Header, Pages 1+)|
+------------------------------+ +----------------------------+
```

---

## 1. Engine Layer (`src/engine`)

### `TreeDb`
* **Role:** High-level unified database interface coordinating the index, record storage, buffer pool, and transactions.
* **Responsibilities:**
  * Key-value operations: `put(key, value)`, `get(key)`, `delete(key)`, `scan(start, end)`.
  * Transaction lifecycle: `begin_txn()`, `commit_txn(txn_id)`, `abort_txn(txn_id)`.
  * Automatic ARIES crash recovery upon opening existing database and WAL files.
  * Explicit buffer pool and WAL synchronization via `sync()`.

---

## 2. WAL & Recovery Layer (`src/wal`)

### `LogManager`
* **Role:** Manages append-only write-ahead logging to the `.wal` file.
* **Responsibilities:**
  * Monotonic LSN allocation and transaction state tracking.
  * Self-delimiting log record serialization with CRC32 integrity verification.
  * Durable file synchronization (`sync_all`) through target LSNs.
  * Sequential log scanning with graceful handling of crash-truncated log tails.

### `RecoveryEngine`
* **Role:** Implements ARIES 3-Phase Crash Recovery (Analysis, Redo, Undo).
* **Responsibilities:**
  * **Analysis Pass:** Reconstructs the Transaction Table and Dirty Page Table to identify loser transactions.
  * **Redo Pass:** Replays history forward for all `PageDelta` and `Clr` records where `page.lsn() < log_record.lsn`.
  * **Undo Pass:** Reverses uncommitted loser transactions backward, writing Compensation Log Records (CLRs) and `Abort` markers.
  * **Idempotence:** Guaranteed safe to run multiple times without corrupting state.

---

## 3. Index Layer (`src/index`)

### `BPlusTree`
* **Role:** Persistent B+ Tree coordinator managing index operations through the `BufferPool`.
* **Responsibilities:**
  * Point lookup (`get`) via root-to-leaf binary search traversal.
  * Dynamic insertion (`insert`) with leaf and internal splits, separator propagation, and root elevation.
  * Dynamic deletion (`delete`) with leaf/internal underflow rebalancing, sibling redistribution, merging, and root collapse.
  * Ordered sequential range scanning (`scan`) utilizing leaf `next_leaf` chains.
  * Safe, leak-free unpinning of all visited pages across every operation path.

### `InternalNode` & `LeafNode`
* **Role:** In-memory structured representation of interior routing and leaf data pages.
* **Responsibilities:**
  * `InternalNode`: Stores up to 339 separator keys routing to 340 children via binary search.
  * `LeafNode`: Stores up to 254 sorted `(Key, RecordId)` entries with sibling links.

### `RecordId`
* **Role:** Fixed-width 8-byte pointer `(page_id: u32, slot_id: u32)` referencing tuples in the storage engine.

---

## 4. Storage Layer (`src/storage`)

### `RecordStore` & `RecordPage`
* **Role:** Persistent variable-length record storage manager and slotted-page layout.
* **Responsibilities:**
  * `RecordStore`: Allocates record pages and manages `insert`, `get`, `delete`, and `compact_page` over `BufferPool`.
  * `RecordPage`: Slotted-page binary layout with bidirectional growth (slot directory grows upward, payloads grow downward), free-space tracking, logical deletion, and in-place payload compaction.

### `BufferPool`
* **Role:** In-memory caching engine maintaining a fixed number of page frames and enforcing Write-Ahead order.
* **Responsibilities:**
  * Fast cache hit lookup via `PageId -> FrameId` page table.
  * Pin count management preventing eviction of actively referenced pages.
  * Dirty page tracking ensuring all modifications are written back before frame reuse.
  * **Write-Ahead Rule Enforcement:** Enforces `LogManager::flush(page.lsn())` before flushing any dirty page to disk.
  * Deterministic Least Recently Used (LRU) replacement policy.

### `PageManager`
* **Role:** Manages reading and writing fixed-size (4096-byte) blocks to and from disk.

---

## 5. Critical Invariants

* **Write-Ahead Rule:** A dirty page must never reach physical disk before its corresponding WAL records (`WAL.flush(page.lsn()) <= page.flush()`).
* **PageLSN Stamping:** Every page modification is stamped with its corresponding log sequence number.
* **No Layer Skipping:** Higher-level structures (`BPlusTree`, `RecordStore`) interact exclusively through the `BufferPool`.
* **Pin Leak Prevention:** All fetched frames must be symmetrically unpinned.
* **Bounded In-Memory Footprint:** The buffer pool never allocates more frames than its configured capacity.
* **Recovery Idempotence:** Multiple recovery passes on a recovered database leave the state identical.
