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
* [x] Leaf node format with derived capacity (255 entries) and sibling links
* [x] Internal routing node format with derived capacity (340 keys / 341 children)
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

## Phase 5: B+ Tree Deletion & Range Scans (Next Phase)
* [ ] Key deletion in leaf nodes
* [ ] Leaf and internal node underflow handling (borrowing / redistribution)
* [ ] Node merging and separator demotion
* [ ] Root collapse on underflow
* [ ] Bidirectional range scan iterator (`scan(start_key..end_key)`)

---

## Phase 6: Slotted Page Record Store & Engine API
* [ ] Slotted-page binary layout for variable-length tuples
* [ ] Record insertion, retrieval, and deletion within pages
* [ ] Slot array management and page defragmentation
* [ ] High-level unified `TreeDb` interface coordinating B+ Tree and Record Store
* [ ] Interactive CLI commands (`get`, `put`, `delete`, `scan`, `stats`)

---

## Phase 7: Durability & Advanced Systems (Future)
* [ ] Write-Ahead Logging (WAL) and log sequence numbers (LSN)
* [ ] ARIES-style crash recovery (Analysis, Redo, Undo)
* [ ] Free-page recycling (freelist / space bitmap)
* [ ] Microbenchmarks and stress testing
