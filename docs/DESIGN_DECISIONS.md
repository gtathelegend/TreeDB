# TreeDB Design Decisions

This document records key architectural and design choices, their rationale, and alternatives considered.

---

## Decision 1: Fixed 4096-Byte (4 KiB) Page Size

* **Decision:** Enforce a constant `PAGE_SIZE = 4096` across the entire storage layer.
* **Rationale:** 
  * 4 KiB aligns with standard OS memory pages and modern SSD block/cluster sizes.
  * Simplifies pointer arithmetic and frame allocation in the Buffer Pool.
  * Matches traditional database storage defaults (SQLite default, etc.).
* **Alternatives Considered:** Dynamic page size configurable per database instance. Rejected for Phase 1 to minimize complexity while establishing storage invariants.

---

## Decision 2: Little-Endian Integer Serialization

* **Decision:** Multi-byte integer fields (such as `PageId`, `Key`, and header metadata) are serialized using explicit little-endian byte ordering (`to_le_bytes()` and `from_le_bytes()`).
* **Rationale:** 
  * Portability across different host architectures (x86_64, ARM64, etc.).
  * Avoids undefined behavior or compiler layout discrepancies from raw struct memory dumps (`std::mem::transmute`).
* **Alternatives Considered:** Big-endian (network byte order). Rejected because modern servers and desktop hardware are almost exclusively little-endian.

---

## Decision 3: Append-Only Page Allocation for Phase 1

* **Decision:** The initial allocator assigns the next sequential `PageId` (`header.page_count`), increments the count, and writes the updated header.
* **Rationale:** 
  * Provides a deterministic and verifiable baseline for disk I/O and persistence without early freelist complexity.
  * Isolates Page Manager responsibilities cleanly before introducing free-page recycling.
* **Alternatives Considered:** Free-space bitmap or linked freelist on disk. Deferred to subsequent phases once page deletion and node merging are implemented.

---

## Decision 4: Dedicated Database Header on Page 0

* **Decision:** Page 0 is reserved strictly for database identification, version verification, and global catalog pointers (`magic`, `format_version`, `page_size`, `page_count`, `root_page_id`, `metadata_page_id`).
* **Rationale:**
  * Fast validation of file integrity upon open before attempting any higher-level operations.
  * Allows backward compatibility detection and future header extension in the reserved 4068 bytes.
* **Alternatives Considered:** Embedding header in the first few bytes of Page 1. Rejected to maintain strict 4 KiB block boundaries for all pages.

---

## Decision 5: Explicit Error Types via `thiserror`

* **Decision:** Use structured enum variants in `treedb::Error` covering all storage conditions (I/O, corruption, page out-of-bounds, version mismatch).
* **Rationale:**
  * Enables callers and higher-level layers to programmatically inspect and handle specific database faults.
  * Avoids loss of context or brittle string matching.

---

## Decision 6: Safe Pinning & Single-Threaded Buffer Pool Ownership

* **Decision:** Implement `BufferPool` with an explicit `fetch_page` / `unpin_page` / `get_page` lifecycle using 100% safe Rust (no `unsafe`, `Mutex`, or `RefCell`).
* **Rationale:**
  * By tracking logical monotonic timestamps on each frame access, LRU eviction is fully deterministic.
  * Clear pin count tracking prevents eviction of actively used frames while avoiding borrow-checker deadlocks.
  * Dirty page state is sticky: clean unpins cannot accidentally clear an already-dirty page.
  * Isolates disk writes exclusively to dirty evictions and explicit flush calls.

---

## Decision 7: B+ Tree Node Layouts and Derived Max Capacities

* **Decision:** Fixed-width binary layouts for Leaf (`[Key (8B) | RecordId (8B)]`) and Internal (`[SeparatorKey (8B) | ChildPageId (4B)]`) nodes, with derived capacities (`LEAF_MAX_ENTRIES = 255`, `INTERNAL_MAX_KEYS = 340`).
* **Rationale:**
  * Eliminates fragmentation and slotted pointer overhead inside index nodes.
  * Maximizes tree fanout (341 children per internal node).
  * Explicit constants derived from page payload boundaries guarantee zero disk buffer overflows.

---

## Decision 8: Routing Convention & RecordId Indirection

* **Decision:** Use standard right-inclusive separator routing (`key < K0 -> C0`, `Ki <= key < Ki+1 -> Ci+1`, `key >= Kn-1 -> Cn`) and map `Key -> RecordId` (where `RecordId = (page_id, slot_id)`).
* **Rationale:**
  * Keeps the B+ Tree index decoupled from variable-sized record/tuple payloads.
  * Enables high-performance binary search routing without page table thrashing.

---

## Decision 9: Minimum Occupancy & Sibling Redistribution Policies

* **Decision:** Define minimum occupancy as `LEAF_MIN_ENTRIES = LEAF_MAX_ENTRIES / 2` (127) and `INTERNAL_MIN_KEYS = INTERNAL_MAX_KEYS / 2` (170). When underflow occurs, borrowing from the left sibling is preferred first, then right sibling, before merging.
* **Rationale:**
  * Strict mathematical derivation from page capacity ensures two underfull nodes can always merge into a single page without exceeding maximum capacity (`126 + 127 = 253 <= 255`).
  * Preferring borrowing over merging minimizes structural tree mutations and page allocations.
  * Deterministic sibling evaluation order guarantees reproducible tree states.

---

## Decision 10: Range Scan API and Leaf Chaining

* **Decision:** Implement `scan(&self, buffer_pool, start, end) -> Result<Vec<(Key, RecordId)>>` with `[start, end)` inclusive/exclusive bounds.
* **Rationale:**
  * Traverses root-to-leaf once to find the lower bound, then sequentially follows `LeafNode.next_leaf` sibling pointers without re-traversing the tree from the root for every key.
  * Yields zero pin leaks by unpinning each leaf frame immediately after copying qualifying records.

---

## Decision 11: Orphaned Page Handling on Merges (No Free-Page Reclamation Yet)

* **Decision:** Pages made unreachable by B+ Tree merges remain orphaned in the database file and are not immediately reclaimed.
* **Rationale:**
  * TreeDB currently uses an append-only page manager. Introducing free-page recycling or freelist bitmaps belongs to a future dedicated storage-management phase.
  * Explicitly documenting this limitation maintains single-responsibility boundaries for Phase 5.

---

## Decision 12: Slotted-Page Record Store & Stable RecordId Indirection

* **Decision:** Implement a slotted-page architecture (`PageType::Record`) where `RecordId = (page_id, slot_id)` serves as an indirect logical reference to variable-length payload bytes.
* **Rationale:**
  * **Decoupled Index & Data:** Keeps B+ Tree nodes purely fixed-width, maximizing index fanout and eliminating memory fragmentation within index nodes.
  * **Stable Logical Identifier:** Because `RecordId` identifies the slot index rather than a byte offset, record payloads can be moved and compacted in-place on the page without modifying or invalidating index entries in the B+ Tree.
  * **Bidirectional Page Layout:** The slot directory grows upward from the page header while record payloads grow downward from the payload end. This maximizes contiguous free space without pre-allocating fixed partition boundaries.
  * **Local Page Compaction:** Payload fragmentation caused by record deletion is defragmented locally on a single page, avoiding multi-page locking or global compaction overhead.
  * **Single-Page Payload Constraint:** For Phase 6, records are constrained to `MAX_RECORD_SIZE = 4078 bytes` (fitting on a single page). Overflow pages and large-object chunking are deferred to future phases.
