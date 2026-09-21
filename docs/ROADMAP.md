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

## Phase 3: Slotted Page Record Store
* [ ] Slotted-page binary layout for variable-length tuples
* [ ] Record insertion, retrieval, and deletion within pages
* [ ] Slot array management and page compaction / defragmentation
* [ ] Record IDs (`RID: (PageId, SlotId)`)

---

## Phase 4: B+ Tree Index Engine
* [ ] B+ Tree leaf and internal node layouts
* [ ] Point search (`get(key) -> Option<Value>`)
* [ ] Key-value insertion and leaf node splitting
* [ ] Internal node splitting and root elevation
* [ ] Deletion with node redistribution and merging
* [ ] Bidirectional range scan iterator (`scan(start..end)`)

---

## Phase 5: High-Level TreeDB Engine & CLI
* [ ] Unified `TreeDb` database handle coordinating buffer pool and index
* [ ] Interactive CLI commands (`get`, `put`, `delete`, `scan`, `stats`, `validate`)
* [ ] Statistics reporting (cache hit ratio, page allocations, tree depth)
* [ ] Integrity validator checking all B+ Tree invariants on disk

---

## Phase 6: Durability & Advanced Systems (Future)
* [ ] Write-Ahead Logging (WAL) and log sequence numbers (LSN)
* [ ] ARIES-style crash recovery (Analysis, Redo, Undo)
* [ ] Free-page recycling (freelist / space bitmap)
* [ ] Microbenchmarks and stress testing
