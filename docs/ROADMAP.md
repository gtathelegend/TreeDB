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
* [x] ARIES-inspired 3-Phase Crash Recovery (Analysis, Redo, Undo with CLRs)
* [x] Crash simulation, idempotence, and truncated/corrupted WAL recovery tests

---

## Phase 7.1: WAL & Crash-Recovery Hardening (Completed)
* [x] Explicit WAL write-ahead ordering verification on both `BufferPool::flush_page` and LRU eviction
* [x] Compensation Log Record (CLR) durability ordering (`append CLR -> flush WAL -> write recovered page`)
* [x] Deterministic crash-during-recovery fault injection hook (`RecoveryFault`)
* [x] Recovery restartability validation across interrupted Undo and Redo passes
* [x] TreeDb high-level transaction-level crash tests (committed/uncommitted `put`, `delete`, and `update`)
* [x] B+ Tree and RecordStore index/data atomicity invariant verification
* [x] Monotonic `TxnId` ownership and `prev_lsn` backlink integrity across operations
* [x] Comprehensive WAL corruption matrix (valid prefix recovery, truncated tail tolerance, mid-log corruption rejection)
* [x] Complete PageLSN comparison behavior matrix (`<`, `==`, `>`, `0`) and non-regression guarantees
* [x] Multi-pass recovery idempotence verification (4+ consecutive recovery passes)
* [x] Storage format version 2 migration tests (v1 rejection with `UnsupportedFormatVersion`, v2 success)
* [x] 4070-byte maximum record boundary validation (`MAX_RECORD_SIZE = 4070 bytes`)
* [x] Zero buffer pool pin leaks verified across all recovery and mutation paths

---

## Phase 8: Free-Page Recycling & Persistent Freelist (Completed)
* [x] Standard reserved page predicates (`is_reserved_page(page_id)`) protecting Page 0 and Page 1
* [x] Persistent singly-linked freelist anchored by `DbHeader::freelist_head` and `DbHeader::free_page_count` on Page 0
* [x] `FreePageLayout` binary format storing `next_free: PageId` in payload
* [x] `FreePageManager` with LIFO push (`free_page`), LIFO pop (`allocate_page`), and double-free protection (`Error::PageAlreadyFree`)
* [x] `PageManager` integration: freelist pop prior to EOF file expansion, and synchronous header persistence
* [x] `BufferPool::allocate_page()` reinitialization: clears `next_free`, zeroes payload, sets target `PageType`, preserves frame safety
* [x] `BufferPool::free_page()`: evicts resident frames, logs `PageDelta` with active transaction to WAL, stamps `page_lsn`, updates `DbHeader`
* [x] B+ Tree integration: leaf merge, internal node merge, root collapse, and empty root transitions reclaim unlinked pages
* [x] Freelist structural integrity validator (`validate_freelist`): verifies total count, page types, cycle detection, reserved/root exclusion
* [x] Crash recovery integration: PageLSN tracking prevents Redo from overwriting recycled or free pages with obsolete states
* [x] 14 dedicated integration tests covering persistence, recycling, bounded file size, B+ Tree reclamation, and pin discipline

---

## Phase 9: Multi-Page Overflow Records (Completed)
* [x] Dedicated overflow page type (`PageType::Overflow = 6`) with explicit 8-byte payload header (`next_page_id: u32`, `bytes_used: u16`, `reserved: u16`)
* [x] Usable overflow payload capacity calculation (`OVERFLOW_PAYLOAD_SIZE = 4072 bytes`)
* [x] In-page overflow descriptor encoding in slotted record pages (`OVERFLOW_FLAG = 0x8000`, 8-byte descriptor `[head_page_id: u32 | total_length: u32]`)
* [x] Stable `RecordId` abstraction maintained for both inline ($\le 4070$ bytes) and overflow ($> 4070$ bytes) records
* [x] RecordStore multi-page allocation, sequential chaining, and transparent value reassembly
* [x] Transparent range scans and CRUD operations across mixed inline and multi-page records
* [x] Size transition handling (small $\to$ small, small $\to$ large, large $\to$ small, large $\to$ large)
* [x] Overflow chain freeing and full persistent freelist recycling integration
* [x] Recycled overflow page reinitialization and zero-leak guarantees
* [x] Slotted record page compaction preserving on-page overflow descriptors and stable `RecordId`s
* [x] Defensive overflow validation: out-of-bounds page detection, partial intermediate page detection, and cycle detection
* [x] WAL/ARIES crash recovery integration with transaction atomicity across index, record slot, and overflow pages
* [x] 17 dedicated overflow integration tests plus randomized oracle comparison

---

## Phase 10: Checkpointing & WAL Lifecycle Management (Completed)
* [x] Synchronous checkpoint protocol (`BufferPool::checkpoint()`, `TreeDb::checkpoint()`)
* [x] Durable checkpoint metadata on Page 0 (`DbHeader::last_checkpoint_lsn` at offset 36..44)
* [x] Checkpoint-aware Analysis pass starting from durable checkpoint and active transaction table
* [x] Dirty Page Table (DPT) tracking `rec_lsn` per page for Redo optimization
* [x] Safe WAL retention boundary formula: $\min(\text{last\_checkpoint\_lsn}, \min(\text{first\_lsn}(T)))$
* [x] Crash-safe atomic WAL prefix truncation and log recycling (`LogManager::truncate_prefix`)
* [x] Loser transaction complete Undo history preservation across checkpoints
* [x] Corrupted checkpoint metadata validation and defense against impossible LSNs
* [x] Freelist and multi-page overflow interaction across checkpoints
* [x] Recovery equivalence testing between full WAL scan and checkpoint recovery
* [x] Demonstrably bounded WAL file growth under periodic checkpointing workloads
* [x] 11 dedicated checkpoint integration tests and benchmarks

---

## Phase 10.1: WAL Lifecycle & Checkpoint Hardening (Completed)
* [x] Formalized and verified fundamental WAL retention invariant: a record is removed only when unreachable from every Redo, Undo, and restart path
* [x] Active transactions spanning multiple checkpoints preserve entire historical Undo chains back to `Begin`
* [x] Strict LSN monotonicity guaranteed after truncation (newly allocated LSNs strictly exceed all pre-truncation LSNs)
* [x] Non-zero WAL starting LSN accepted seamlessly by recovery scanner
* [x] Seamless append operations immediately following prefix truncation without offset or byte corruptions
* [x] Crash-safe temporary WAL replacement and deterministic `.wal.bak` fallback recovery on Windows filesystem
* [x] Comprehensive checkpoint metadata & record validation (detects and rejects impossible LSNs and corrupted active txn entries with structured `Error::WalCorruption`)
* [x] Validated PageLSN older than truncated WAL (flushed durable pages with `PageLSN < first_retained_lsn` are valid)
* [x] Free-page and multi-page overflow recycling safety across multiple checkpoint/truncation cycles
* [x] Repeated multi-cycle checkpoint/truncation idempotence testing (25+ continuous cycles)
* [x] Four-path recovery equivalence matrix (No Checkpoint, Checkpoint Only, Checkpoint + Truncate, Checkpoint + Truncate + Crash)
* [x] 21 dedicated checkpoint and lifecycle hardening tests (178 total test suite)

---

## Phase 11: Group Commit, Concurrency & CLI (Future)
* [ ] Group commit and asynchronous WAL flush queues
* [ ] Interactive CLI commands (`get`, `put`, `delete`, `scan`, `stats`, `inspect-freelist`)
* [ ] Microbenchmarks and high-throughput stress testing
* [ ] Multi-threading, concurrency control, and latching




