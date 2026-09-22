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
  * Monotonic LSN allocation and transaction state tracking (`TxnId`, `prev_lsn`).
  * Self-delimiting log record serialization with IEEE 802.3 CRC32 integrity verification.
  * Durable file synchronization (`sync_all`) through target LSNs (`flushed_lsn`).
  * Sequential log scanning distinguishing clean truncated crash tails from mid-log corruption (`Error::WalCorruption`).

### `RecoveryEngine` & `Checkpointing`
* **Role:** Implements an ARIES-inspired recovery pipeline and synchronous checkpoint protocol.
* **Responsibilities:**
  * **Synchronous Checkpoint:** Flushes all dirty buffer pool frames (enforcing Write-Ahead ordering), captures active transactions in a `Checkpoint` WAL record, flushes WAL, and persists `last_checkpoint_lsn` to Page 0 (`DbHeader`).
  * **Safe WAL Prefix Truncation:** Computes the retention boundary $\min(\text{last\_checkpoint\_lsn}, \min(\text{first\_lsn}(T)))$ and safely removes obsolete WAL prefixes with atomic temporary file swapping.
  * **Checkpoint-Aware Analysis Pass:** Initializes the active transaction table directly from the durable checkpoint, starting log scan from the earliest active transaction LSN.
  * **DPT & Redo Pass Optimization:** Reconstructs Dirty Page Table (`rec_lsn` per page) to bypass unnecessary page reads during Redo. Replays forward for all `PageDelta` and `Clr` records where `page.lsn() < log_record.lsn()`.
  * **Undo Pass:** Reverses uncommitted loser transactions backward in reverse LSN order, writing Compensation Log Records (CLRs) with `undo_next_lsn` pointers and `Abort` markers.
  * **CLR Durability Order:** Enforces `append CLR -> flush WAL through CLR LSN -> write recovered page` so recovery modifications never precede their CLR on disk.
  * **Fault Injection & Restartability:** Supports deterministic interruption via `RecoveryFault` (before recovery, during Redo, during Undo, after CLR append, after CLR flush, after page write). Recovery safely restarts, redoing durable CLRs and continuing Undo.
  * **Multi-Pass Idempotence:** Multiple consecutive recovery passes produce byte-identical database files with no pin leaks or PageLSN regressions.

---

## 3. Index Layer (`src/index`)

### `BPlusTree`
* **Role:** Persistent B+ Tree coordinator managing index operations through the `BufferPool`.
* **Responsibilities:**
  * Point lookup (`get`) via root-to-leaf binary search traversal.
  * Dynamic insertion (`insert`) with leaf and internal splits, separator propagation, and root elevation.
  * In-place key update (`update`) modifying existing key mappings without allocating or collapsing nodes.
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

### `RecordStore`, `RecordPage` & `OverflowPage`
* **Role:** Persistent variable-length record storage manager, slotted-page layout, and multi-page overflow chains.
* **Responsibilities:**
  * `RecordStore`: Coordinates `insert`, `get`, `delete`, `compact_page`, and `validate_overflow_chain` over `BufferPool`.
  * `RecordPage`: Slotted-page binary layout with bidirectional growth (slot directory grows upward, payloads/descriptors grow downward), free-space tracking, logical deletion, and in-place payload compaction.
  * `OverflowPage`: Singly-linked overflow page format (`PageType::Overflow`) storing up to 4072 bytes per page (`OVERFLOW_PAYLOAD_SIZE`).
  * **Inline vs Overflow Storage:**
    * Values $\le \text{MAX\_RECORD\_SIZE}$ (4070 bytes) are stored inline in slotted record pages.
    * Values $> 4070$ bytes are stored across a singly-linked chain of `Overflow` pages, referenced by an 8-byte descriptor `[head_page_id: u32 | total_length: u32]` on a `RecordPage`.
    * `RecordId { page_id, slot_id }` remains stable and opaque to callers and the B+ Tree index.
  * **Deletion & Recycling:** Deleting an overflow-backed record frees all pages in its overflow chain directly into the persistent freelist for subsequent reuse.

### `BufferPool`
* **Role:** In-memory caching engine maintaining a fixed number of page frames and enforcing Write-Ahead order.
* **Responsibilities:**
  * Fast cache hit lookup via `PageId -> FrameId` page table.
  * Pin count management preventing eviction of actively referenced pages.
  * Dirty page tracking ensuring all modifications are written back before frame reuse.
  * **Automatic Physical Delta Logging:** Tracks `before_image` when frames are modified; generates and appends `PageDelta` log records upon frame unpinning.
  * **Write-Ahead Rule Enforcement:** Enforces `LogManager::flush(page.lsn())` before flushing any dirty page to disk on both explicit `flush_page()` and `LRU eviction`.
  * Deterministic Least Recently Used (LRU) replacement policy.

### `FreePageManager` & `FreePageLayout`
* **Role:** Manages the persistent singly-linked freelist anchored by `DbHeader::freelist_head` on Page 0.
* **Responsibilities:**
  * LIFO free-page stack push (`free_page`) and pop (`allocate_page`).
  * Explicit page lifecycle management: `NEW -> ALLOCATED -> REACHABLE -> UNLINKED -> FREE -> REUSED -> ALLOCATED`.
  * Reinitialization on reuse: zeroes the entire 4080-byte payload, clears `next_free`, and sets the target `PageType`.
  * Freelist validation (`validate`): checks total count matches `DbHeader::free_page_count`, verifies all nodes have `PageType::Free`, detects cycles via Floyd's tortoise-and-hare algorithm, and enforces that reserved pages (Page 0 and Page 1) and active roots are never present.

---

## 5. Critical Invariants

* **Write-Ahead Invariant:** `append WAL record -> WAL durable -> page modification -> page flush`. A dirty page with `page_lsn > wal.flushed_lsn` cannot reach disk without its WAL records first being flushed.
* **PageLSN Semantics:** Every page modification stamps the page with its corresponding LSN. During Redo:
  * `page.lsn() < log_record.lsn()` => Redo applied.
  * `page.lsn() >= log_record.lsn()` => Redo skipped.
  PageLSN never regresses.
* **CLR Durability Invariant:** `append CLR -> flush WAL through CLR LSN -> write recovered page`. Recovery-generated modifications are never durable before their CLR is durable on disk.
* **Engine Atomicity Boundary:** `TreeDb::put` and `TreeDb::delete` operations wrap both RecordStore heap modifications (inline and overflow) and B+ Tree index mutations in a single transaction (`TxnId`). If an uncommitted operation crashes, both index and record mutations are undone atomically.
* **WAL Corruption Policy:** Valid log prefixes are recovered. Incomplete trailing crash tails are safely ignored. Corruptions in the middle of durable log records return `Error::WalCorruption`.
* **Zero Pin Leaks:** All buffer pool frames must have `pin_count == 0` after every normal operation, failed recovery, successful recovery, or repeated recovery pass.
* **Recovery Restartability & Idempotence:** Recovery can be safely interrupted at any point (before/during Redo, during Undo, after CLRs) and restarted. Repeated recovery passes yield identical database states.
* **Page Reachability & Reservation Invariant:** Page 0 (Header) and Page 1 (Metadata) are strictly reserved (`is_reserved_page(page_id) == true`) and can never be freed or recycled. Every page ID in the database is either reachable via index/metadata/heap or tracked in the persistent freelist.

---

## 6. Intentionally Deferred Items (Phase 11+)

The following capabilities are intentionally deferred to subsequent phases and not implemented in Phase 10:
* Background fuzzy checkpointing and asynchronous log worker threads.
* Group commit and lock-free WAL flush rings.
* Multi-threading, concurrency control, and latching.
* SQL query parser, execution engine, and MVCC.

