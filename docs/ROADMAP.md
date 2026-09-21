# TreeDB Implementation Roadmap

This roadmap outlines the planned development phases for TreeDB.

---

## Phase 1: Foundation & Storage Layer (Completed)
* [x] Project architecture and directory structure
* [x] Standard storage constants (`PAGE_SIZE = 4096`, `PageId`, `Key`)
* [x] Page abstraction and structural validation
* [x] Database file header (Page 0) and metadata layout (Page 1)
* [x] Structured error hierarchy (`thiserror`)
* [x] `PageManager` disk I/O, append-only page allocation, and file synchronization
* [x] Persistence and corrupt/truncated file test suite
* [x] CI workflow with formatting, clippy, and unit/integration tests

---

## Phase 2: Buffer Pool Manager (Completed)
* [x] In-memory frame table and page descriptor state (`FrameId`, `Frame`)
* [x] Page table mapping (`PageId -> FrameId`)
* [x] Pinning / unpinning lifecycle mechanics and underflow protection
* [x] Dirty page tracking, sticky dirty state, and explicit/eviction flushing
* [x] Deterministic LRU replacement policy protecting pinned pages
* [x] Buffer pool capacity limits and `NoEvictablePage` handling
* [x] Buffer pool metrics and hit rate computation
* [x] Comprehensive test suite covering single-capacity boundaries and persistence

---

## Phase 3: B+ Tree Node Format & Read-Only Search (Completed)
* [x] Fixed-width `RecordId` representation (`(PageId, SlotId)`)
* [x] Leaf node format with derived capacity (254 entries) and sibling links
* [x] Internal routing node format with derived capacity (339 keys / 340 children)
* [x] Right-inclusive separator routing convention
* [x] Deterministic little-endian serialization and bounds/sorting validation
* [x] `BPlusTree` handle with point lookup (`get(key) -> Option<RecordId>`)
* [x] Zero-leak buffer pool pinning discipline during tree traversal
* [x] Multi-level tree traversal and database reopen persistence test suite

---

## Phase 4: B+ Tree Insertion, Splitting & Root Growth (Completed)
* [x] Insertion into non-full leaf nodes
* [x] Duplicate key policy (`Error::DuplicateKey`)
* [x] Leaf node splitting and sibling pointer chaining
* [x] Separator key promotion to parent internal nodes
* [x] Internal node splitting and recursive parent updates
* [x] Root node splitting and root elevation
* [x] Root page metadata persistence on Page 0
* [x] Zero pin leak verification across multi-level splits
* [x] Structural invariant validator (`validate_tree`)

---

## Phase 5: B+ Tree Deletion & Range Scans (Completed)
* [x] Key deletion in leaf nodes (`delete(key) -> Option<RecordId>`)
* [x] Leaf underflow detection and sibling borrowing / redistribution
* [x] Leaf node merging and sibling link updates
* [x] Internal node underflow detection, separator rotation, and child borrowing
* [x] Internal node merging with parent separator demotion
* [x] Recursive path rebalancing toward root
* [x] Root collapse (height reduction and empty tree transition)
* [x] Empty tree handling and re-insertion safety
* [x] Root metadata persistence across database reopen
* [x] Ordered range scans (`scan(start, end)`) via leaf sibling chains
* [x] Deterministic randomized mutation testing vs BTreeMap oracle
* [x] Zero pin leak verification across all mutation paths

---

## Phase 6: Slotted Page Record Store & Engine API (Completed)
* [x] Slotted-page binary layout for variable-length tuples (`RecordPage`)
* [x] Bidirectional free-space tracking (upward slot directory, downward payloads)
* [x] Record insertion, retrieval, and deletion within pages
* [x] In-place page-level payload compaction with stable `RecordId` references
* [x] Slot reuse on deletion
* [x] Multi-page record store management (`RecordStore`)
* [x] High-level unified `TreeDb` interface coordinating B+ Tree and Record Store (`put`, `get`, `delete`, `scan`, `sync`)
* [x] Deterministic randomized mutation testing with reference oracle
* [x] Multi-page allocation and reopen persistence test suite

---

## Phase 7: Write-Ahead Logging & Crash Recovery (Completed)
* [x] Monotonic Log Sequence Numbers (`Lsn`) and Transaction IDs (`TxnId`)
* [x] Separate WAL file (`.wal`) with explicit `TREEWAL1` header
* [x] Self-delimiting log record format with CRC32 integrity checksums
* [x] Physical page delta logging (`before_image`, `after_image`)
* [x] 16-byte standard page headers storing `PageLSN` (format version 2)
* [x] Buffer Pool Write-Ahead rule enforcement (`WAL.flush(page.lsn()) <= page.flush()`)
* [x] ARIES 3-Phase Crash Recovery (Analysis, Redo, Undo with CLRs)
* [x] Crash simulation, idempotence, and truncated/corrupted WAL recovery tests

---

## Phase 8: Advanced Storage, Recycling & CLI (Next Phase)
* [ ] Free-page recycling across the database (freelist / bitmap)
* [ ] Multi-page overflow records for large BLOBs
* [ ] Periodic background fuzzy checkpointing
* [ ] Interactive CLI commands (`get`, `put`, `delete`, `scan`, `stats`)
* [ ] Microbenchmarks and stress testing
