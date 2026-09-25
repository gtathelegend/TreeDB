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

* **Decision:** Fixed-width binary layouts for Leaf (`[Key (8B) | RecordId (8B)]`) and Internal (`[SeparatorKey (8B) | ChildPageId (4B)]`) nodes, with derived capacities (`LEAF_MAX_ENTRIES = 254`, `INTERNAL_MAX_KEYS = 339`).
* **Rationale:**
  * Eliminates fragmentation and slotted pointer overhead inside index nodes.
  * Maximizes tree fanout (340 children per internal node).
  * Explicit constants derived from page payload boundaries guarantee zero disk buffer overflows.

---

## Decision 8: Routing Convention & RecordId Indirection

* **Decision:** Use standard right-inclusive separator routing (`key < K0 -> C0`, `Ki <= key < Ki+1 -> Ci+1`, `key >= Kn-1 -> Cn`) and map `Key -> RecordId` (where `RecordId = (page_id, slot_id)`).
* **Rationale:**
  * Keeps the B+ Tree index decoupled from variable-sized record/tuple payloads.
  * Enables high-performance binary search routing without page table thrashing.

---

## Decision 9: Minimum Occupancy & Sibling Redistribution Policies

* **Decision:** Define minimum occupancy as `LEAF_MIN_ENTRIES = LEAF_MAX_ENTRIES / 2` (127) and `INTERNAL_MIN_KEYS = INTERNAL_MAX_KEYS / 2` (169). When underflow occurs, borrowing from the left sibling is preferred first, then right sibling, before merging.
* **Rationale:**
  * Strict mathematical derivation from page capacity ensures two underfull nodes can always merge into a single page without exceeding maximum capacity (`126 + 127 = 253 <= 254`).
  * Preferring borrowing over merging minimizes structural tree mutations and page allocations.
  * Deterministic sibling evaluation order guarantees reproducible tree states.

---

## Decision 10: Range Scan API and Leaf Chaining

* **Decision:** Implement `scan(&self, buffer_pool, start, end) -> Result<Vec<(Key, RecordId)>>` with `[start, end)` inclusive/exclusive bounds.
* **Rationale:**
  * Traverses root-to-leaf once to find the lower bound, then sequentially follows `LeafNode.next_leaf` sibling pointers without re-traversing the tree from the root for every key.
  * Yields zero pin leaks by unpinning each leaf frame immediately after copying qualifying records.

---

## Decision 11: Free-Page Reclamation on B+ Tree Merges & Collapses
 
* **Decision:** Pages made unreachable by B+ Tree leaf merges, internal node merges, root collapses, and empty tree transitions are actively freed via `BufferPool::free_page()` and returned to the persistent freelist.
* **Rationale:**
  * Avoids dead-space fragmentation and unbounded database file growth under heavy delete workloads.
  * Reclaims freed space immediately for subsequent page allocations across both index and record store components.

---

## Decision 12: Slotted-Page Record Store & Stable RecordId Indirection


* **Decision:** Implement a slotted-page architecture (`PageType::Record`) where `RecordId = (page_id, slot_id)` serves as an indirect logical reference to variable-length payload bytes.
* **Rationale:**
  * **Decoupled Index & Data:** Keeps B+ Tree nodes purely fixed-width, maximizing index fanout and eliminating memory fragmentation within index nodes.
  * **Stable Logical Identifier:** Because `RecordId` identifies the slot index rather than a byte offset, record payloads can be moved and compacted in-place on the page without modifying or invalidating index entries in the B+ Tree.
  * **Bidirectional Page Layout:** The slot directory grows upward from the page header while record payloads grow downward from the payload end. This maximizes contiguous free space without pre-allocating fixed partition boundaries.
  * **Local Page Compaction:** Payload fragmentation caused by record deletion is defragmented locally on a single page, avoiding multi-page locking or global compaction overhead.
  * **Single-Page Payload Constraint:** Records are constrained to `MAX_RECORD_SIZE = 4070 bytes` (fitting on a single page). Overflow pages and large-object chunking are deferred to future phases.

---

## Decision 13: Write-Ahead Logging (WAL), PageLSN, and ARIES-Inspired Crash Recovery

* **Decision:** Implement a dedicated write-ahead logging subsystem with monotonic `Lsn` sequence numbers, 16-byte standard page headers storing `PageLSN`, physical page delta logging, and an ARIES-inspired 3-Phase recovery pipeline (Analysis, Redo, Undo with CLRs).
* **Rationale:**
  * **Write-Ahead Rule:** Enforces that dirty database pages are never written to physical disk before their corresponding log records are durably synced (`WAL.flush(page.lsn()) <= page.flush()`).
  * **Clean Storage Format Versioning:** Expanding page headers from 8 to 16 bytes to house the 64-bit `PageLSN` increments format version from `1` to `2`, cleanly rejecting unversioned files without silent misinterpretation.
  * **Physical Page Deltas:** Logging byte ranges (`offset`, `before_image`, `after_image`) keeps the WAL completely decoupled from higher-level B+ Tree and Record Store mechanics.
  * **ARIES-Inspired 3-Phase Recovery:**
    * *Analysis Pass:* Reconstructs active transaction table and dirty page table, identifying loser transactions.
    * *Redo Pass:* Repeats history from the earliest unreflected LSN, skipping pages where `page.lsn >= record.lsn`.
    * *Undo Pass:* Rolls back loser transactions backward, writing Compensation Log Records (CLRs) to ensure crash-during-recovery is fully idempotent and restart-safe.

---

## Decision 14: WAL Ordering Invariant on Explicit Flush and LRU Eviction

* **Decision:** In `BufferPool`, any frame written to disk (whether through explicit `flush_page` / `flush_all` or through `evict_lru_frame`) must first call `wal.flush(page.lsn())`.
* **Rationale:**
  * Proves the fundamental durability invariant: `append WAL record -> WAL durable -> page modification -> page flush`.
  * If a dirty frame with `page_lsn > wal.flushed_lsn` is written to disk during LRU eviction or explicit flush, uncommitted or committed changes could exist on disk without durable WAL records, violating recoverability.
  * Tying the WAL flush directly into `BufferPool::flush_frame_internal` guarantees that no page modification ever bypasses the WAL.

---

## Decision 15: Compensation Log Record (CLR) Durability Order

* **Decision:** During the Undo phase of crash recovery, when a page modification is reversed:
  1. Append the CLR to the log manager (`wal.append_clr(...)`).
  2. Durably flush the WAL through the CLR's LSN (`wal.flush(clr_lsn)`).
  3. Write the recovered page to the database file (`page_manager.write_page(...)`).
* **Rationale:**
  * A recovery-generated page modification must never become durable on disk before its CLR is durable in the log.
  * If the system crashes after the page is written but before the CLR is durable, subsequent recovery would not know the undo had already been applied, risking double-undo or state corruption.
  * Enforcing CLR flushing before writing recovered pages guarantees recovery restartability.

---

## Decision 16: Deterministic Recovery Fault Injection & Restartability

* **Decision:** Provide a testable hook `RecoveryEngine::recover_with_fault(..., fault: RecoveryFault)` where `RecoveryFault` can inject interruptions before recovery, during Redo, during Undo, after CLR append, after CLR flush, or after recovered page writes.
* **Rationale:**
  * Avoids brittle, non-deterministic OS process termination in tests.
  * Allows comprehensive verification that recovery is restart-safe: when interrupted during Undo, restarting recovery redos the durable CLRs, follows `undo_next_lsn` pointers, and cleanly rolls back remaining loser transactions to a consistent state.

---

## Decision 17: WAL Corruption Policy (Valid Prefix vs. Mid-Log Corruption)

* **Decision:**
  * Incomplete log records at the end of the log (e.g., partial tail writes from a crash) are safely ignored if they occur after the durable `flushed_lsn`.
  * Mid-log corruptions (CRC mismatch, invalid record tag, payload length overflow, LSN non-monotonicity within the durable prefix) return `Error::WalCorruption`.
* **Rationale:**
  * Distinguishes expected trailing garbage from catastrophic mid-log corruption.
  * A corrupted middle record indicates media or logical corruption and must NOT silently truncate valid subsequent history.

---

## Decision 18: In-Place B+ Tree Key Update for High-Level Engine Atomicity

* **Decision:** Implement `LeafNode::update` and `BPlusTree::update` to update the `RecordId` of an existing key in-place, and use this in `TreeDb::put` when replacing existing values.
* **Rationale:**
  * Deleting and re-inserting a key inside `put` risks collapsing a 1-key tree into an empty root (`INVALID_PAGE_ID`) and re-allocating a new root page within the same transaction.
  * In-place update keeps the B+ Tree structure stable and avoids unnecessary page allocations and root modifications while preserving transaction ownership (`TxnId`, `prev_lsn`) across index and record store mutations.

---

## Decision 19: Persistent Singly-Linked Freelist with LIFO Allocation Policy

* **Decision:** Implement free page tracking using an on-disk singly-linked list anchored by `DbHeader::freelist_head` and `DbHeader::free_page_count` on Page 0, where each free page (`PageType::Free`) stores its `next_free: PageId` in the first 4 bytes of its payload.
* **Rationale:**
  * **$O(1)$ Operations:** Pushing a freed page and popping a reusable page take constant time ($O(1)$) without scanning the database file or maintaining large in-memory bitmaps.
  * **Minimal Overhead:** Fits cleanly into the existing page structure without requiring auxiliary allocation files or dynamic resizing structures.
  * **Cache Locality:** LIFO reuse favors recently freed pages that are likely still warm in operating system disk caches.

---

## Decision 20: Explicit Zero-Leak Reinitialization on Reuse

* **Decision:** When a page is popped from the freelist in `BufferPool::allocate_page()`, the entire 4080-byte payload is zeroed out, `next_free` is cleared, and `page_type` is initialized to the requested target type before returning the pinned frame.
* **Rationale:**
  * Prevents stale data remnants from previous lifecycles leaking into new index or record pages.
  * Ensures deterministic page state and reproducible binary representations.

---

## Decision 21: WAL Delta Logging on Page Free Operations

* **Decision:** When `BufferPool::free_page(page_id)` is invoked with an active transaction context, it constructs a `PageDelta` capturing the transition from the page's prior content to `PageType::Free`, stamps the free page's `page_lsn`, and updates the header.
* **Rationale:**
  * Maintains the fundamental ARIES invariant that all physical state transitions are recorded in the WAL.
  * Stamping `page_lsn` prevents Redo replay from incorrectly applying older pre-free log records over a freed/recycled page.

---

## Decision 22: Multi-Page Overflow Records and Capacity Derivation

* **Decision:** Values exceeding `MAX_RECORD_SIZE = 4070 bytes` are stored across a singly-linked chain of `PageType::Overflow` pages (`OVERFLOW_PAYLOAD_SIZE = 4072 bytes` per page) referenced by an 8-byte descriptor (`head_page_id: u32`, `total_length: u32`) in a `RecordPage` slot with bit 15 (`OVERFLOW_FLAG = 0x8000`) set in `Slot.length`.
* **Rationale:**
  * **Opaque Abstraction:** `RecordId { page_id, slot_id }` remains unchanged and stable; callers and the B+ Tree index never need to know whether a record is inline or overflow-backed.
  * **Backward Compatibility:** All existing inline records (`length <= 4070`) have bit 15 clear, allowing seamless reading and writing of existing format version 2 files without migrations.
  * **Explicit Little-Endian Serialization:** Overflow headers (`next_page_id: u32`, `bytes_used: u16`, `reserved: u16`) are strictly little-endian encoded.
  * **Freelist Integration:** Deleting an overflow record reclaims all pages in its chain directly into the persistent freelist.
  * **Defensive Integrity Validation:** Bounded traversal and cycle detection protect against infinite loops in corrupted databases.

---

## Decision 23: Synchronous Checkpointing Protocol & Durable Metadata

* **Decision:** Synchronous checkpoints execute in a strict multi-step order:
  1. `BufferPool::flush_all()` flushes all resident dirty pages to disk, enforcing the WAL Write-Ahead invariant (`PageLSN <= durable WAL LSN`) for every page before disk write.
  2. Query active uncommitted transactions and append a `Checkpoint { active_txns }` record to the WAL.
  3. Flush WAL to physical storage through the checkpoint LSN.
  4. Persist `last_checkpoint_lsn` to Page 0 (`DbHeader` offset 36..44) and sync the database file.
* **Rationale:**
  * **Strict Durability Invariant:** A checkpoint is never marked durable until both dirty pages and the WAL checkpoint record are on disk, and the header metadata is synced.
  * **Single-Threaded Simplicity:** Synchronous checkpoints avoid background worker threads and asynchronous runtime dependencies while guaranteeing exact recovery boundaries.

---

## Decision 24: WAL Retention Boundary & Crash-Safe Atomic Truncation

* **Decision:** The safe WAL retention boundary is computed as:
  $$\text{retention\_lsn} = \min\left( \text{last\_checkpoint\_lsn},\; \min_{T \in \text{ActiveTxns}} (\text{first\_lsn}(T)) \right)$$
  WAL prefix truncation writes retained records (`LSN >= retention_lsn`) to a temporary file (`.wal.tmp`), syncs to disk, releases file handles, renames the existing file to `.wal.bak`, renames `.wal.tmp` to `.wal`, and cleans up `.wal.bak`.
* **Rationale:**
  * **Undo Chain Preservation:** Active loser transactions spanning checkpoints require their complete historical Undo chains (back to `Begin`). Truncating only before `retention_lsn` guarantees loser transactions can always be undone.
  * **Cross-Platform Crash-Safety:** Releasing file handles and using atomic renames prevents Windows sharing violations and ensures that even if a crash occurs during truncation, either the original or the replacement WAL is completely valid.

---

## Decision 25: Deterministic Backup WAL Fallback and Hardened Lifecycle Semantics

* **Decision:** When opening the WAL (`LogManager::open`):
  1. If `.wal` is missing or truncated below `WAL_HEADER_SIZE` (32 bytes) but `.wal.bak` exists, `.wal.bak` is atomically restored to `.wal` and used for recovery.
  2. If both `.wal` and `.wal.bak` are missing or corrupted, structured error `Error::WalCorruption` is returned.
  3. LSN allocation maintains strict global monotonicity across file truncations: newly allocated LSNs are always $> \text{max}(\text{retained LSNs}, \text{pre-truncation LSNs})$.
  4. Non-zero starting LSNs are explicitly accepted: recovery scans from the first retained record and does not require LSN 1 to be present.
* **Rationale:**
  * Handles all crash interruption scenarios during atomic WAL replacement on Windows filesystems without data loss.
  * Guarantees that truncated WAL suffixes can be immediately appended to and reopened without LSN collisions or offset confusion.

---

## Decision 26: Explicit Transaction Abstraction, Lifecycle State Machine & CLR-Based Runtime Rollback

* **Decision:** Introduce `Transaction<'db>` with an explicit state machine (`Active`, `Committed`, `Aborted`), mapping directly to a single WAL `TxnId`. Runtime `abort()` (and `Drop` of an active transaction) performs a full physical rollback by following the transaction's `prev_lsn` chain in reverse order, applying `before_image` byte diffs, writing Compensation Log Records (CLRs), and flushing the WAL.
* **Rationale:**
  * **Unified Recovery Invariant:** Runtime abort and crash-recovery undo share identical ARIES CLR principles: every undo operation produces durable CLR records, guaranteeing idempotence even if a crash interrupts runtime abort.
  * **Safe Rust Ownership:** `Transaction<'db>` borrows `&'db mut TreeDb`, statically preventing concurrent access while dynamic tracking (`Error::TransactionActive`) rejects attempts to begin nested transactions.
  * **Read-Your-Writes:** Modifications directly update the buffer pool and index, ensuring immediate read visibility within the same transaction without duplicate buffering layers.

---

## Decision 27: Persistent Catalog Design, Monotonic TableIds & Schema Metadata

* **Decision:** Implement a persistent catalog rooted on Page 1 (`DEFAULT_METADATA_PAGE_ID`) utilizing dedicated catalog pages (`PageType::Catalog = 7` or `PageType::Metadata`) with self-describing 32-byte headers (`CATL` magic, version 1, `next_table_id`, `table_count`, `next_catalog_page_id`). Table entries store stable monotonic `TableId`s (never reused upon drop), `schema_version = 1`, `root_page_id`, and `record_page_id`.
* **Rationale:**
  * **Layer Decoupling:** Logical table metadata is cleanly isolated from the physical B+ Tree and RecordStore implementations, preparing the storage engine for future table/query layers without modifying lower-level formats.
  * **Unbounded Capacity via Paged Chaining:** Catalog pages link sequentially via `next_catalog_page_id`, supporting arbitrary table counts without imposing small single-page limits.
---

## Decision 28: Typed Tables, Relational Tuple Storage & Canonical Primary Key Indexing

* **Decision:** Introduce a typed relational schema layer (`DataType`, `Value`, `Column`, `Schema`, `Row`, `Table<'a>`) backed by deterministic binary tuple serialization and canonical primary key indexing:
  1. **Logical Type System:** Seven core primitives (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`) with explicit 1-byte discriminants and typed scalar values.
  2. **Canonical Primary Key Mapping:** To index signed 64-bit integers (`DataType::Int64`) in the unsigned B+ Tree (`Key = u64`), map `i64` via sign-bit inversion: `(i as u64) ^ (1 << 63)`. This maps `[i64::MIN, i64::MAX]` monotonically onto `[0, u64::MAX]`, ensuring negative integers sort naturally before positive integers during search and range scans.
  3. **Deterministic Binary Tuple Encoding (Version 1):** Serializes a row as `[format_version: u8 = 1, column_count: u16, null_bitmap: [u8; ceil(N/8)], packed_payloads...]`. Variable-length strings and byte blobs are length-prefixed with 4-byte little-endian integers.
  4. **Dynamic Metadata Synchronization:** Table handles (`Table<'a>`) automatically synchronize catalog root page pointers whenever root splitting or merging occurs in the underlying B+ Tree, ensuring table reopen always resolves the latest root page.
* **Rationale:**
  * **Zero Abstraction Cost:** Relational tuple encoding transparently utilizes the existing slotted `RecordStore` and multi-page overflow mechanisms without altering page header formats or introducing external serialization crates.
  * **Order-Preserving Indexing:** Canonical integer transformations allow signed and unsigned integers to share the high-performance B+ Tree index engine without redundant specialized index implementations.
  * **Strict Type Safety:** Schema validation occurs prior to binary encoding, preventing corrupted or malformed tuples from entering physical storage.

---

## Decision 29: Programmatic Query Execution, Expression AST & Three-Valued Logic Engine

* **Decision:** Introduce a composable relational query execution engine and scalar expression evaluator on top of the typed `Table` API:
  1. **Scalar Expression AST & Evaluation (`src/query/expr.rs`):** `Expr` AST supporting literals, column references, unary operators (`NOT`), binary operators (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`), and null predicates (`IS NULL`, `IS NOT NULL`).
  2. **SQL-Compliant Three-Valued Logic (3VL):** Boolean operators (`AND`, `OR`, `NOT`) adhere to standard SQL 3VL truth tables where `UNKNOWN` is represented by `Value::Null`. Comparisons involving `NULL` evaluate to `Value::Null`, and `FilterOperator` retains rows strictly when the predicate evaluates to `Value::Boolean(true)` (discarding `false` and `Null`).
  3. **Multi-Key Sorting & Deterministic NULL Ordering:** `SortOperator` supports multi-key ordering with configurable NULL placement (`nulls_first: Option<bool>`), defaulting to NULLs first in `ASC` and NULLs last in `DESC`. Floating-point numbers are ordered using `f64::total_cmp`, ensuring deterministic handling of `NaN` and signed zeros.
  4. **Primary Key Point & Range Scan Optimizations:** `Query` inspects filter predicates to recognize single-point lookups (`pk == lit`) and range constraints (`pk >= low AND pk < high`), executing direct B+ Tree point lookups or range scans and eliminating unnecessary table scans while maintaining 100% equivalence with full-scan evaluation.
  5. **Programmatic Builder & Schema Propagation:** `Query` builder provides a fluent builder API (`filter`, `project`, `sort`, `offset`, `limit`) propagating derived `Schema` metadata through each pipeline stage without relying on SQL text parsing.
* **Rationale:**
  * **Clean Decoupling:** Separating query execution and expression evaluation from future SQL parsing allows relational semantics and optimizations to be verified independently before introducing a parser.
  * **Zero External Dependencies:** Built entirely with safe, idiomatic standard Rust abstractions and structured error handling.
  * **Transaction-Aware Execution:** Queries execute transparently over active `Transaction` handles, observing uncommitted mutations and respecting transaction rollbacks.

---

## Decision 30: Scoped SQL Lexer, Parser & Independent AST Representation

* **Decision:** Implement a zero-dependency hand-written lexer, precedence-climbing (Pratt) expression parser, and abstract syntax tree (`src/sql/`) strictly targeting the scoped `SELECT` dialect:
  1. **Target Dialect:** `SELECT [wildcard | expressions [AS alias]] FROM table [WHERE expr] [ORDER BY expr [ASC | DESC], ...] [LIMIT n] [OFFSET n];`.
  2. **Lexer & Span Tracking (`src/sql/lexer.rs`, `src/sql/span.rs`):** Hand-written scanner emitting `Token { kind, span }`. Tracks exact byte offsets (`SourceSpan { start, end }`) for informative syntax error diagnostics. Implements case-insensitive keyword recognition, case-sensitive identifiers (enforcing `MAX_IDENTIFIER_LEN = 128`), single-quoted string literals with SQL-standard `''` escaping, signed integers/floats, booleans (`TRUE`/`FALSE`), `NULL`, and single-line comments (`-- ...`).
  3. **Pratt Precedence Expression Parser (`src/sql/parser.rs`):** Implements precedence climbing across 6 distinct levels: `Parentheses` > `Unary (NOT, -)` > `IS [NOT] NULL` > `Comparisons (=, !=, <>, <, <=, >, >=)` > `AND` > `OR`. Normalizes inequality operators (`!=` and `<>`) to `SqlBinaryOp::NotEq`.
  4. **Decoupled AST (`src/sql/ast.rs`):** Fully decoupled `SelectStatement` AST independent of disk storage, buffer pools, transaction managers, catalogs, or runtime expression evaluation (`Expr`).
  5. **Standalone API (`src/sql/mod.rs`):** Exposes `treedb::sql::parse(sql)` and `treedb::sql::tokenize(sql)` which can parse arbitrary valid SQL queries in isolation without opening a database file.
* **Rationale:**
  * **Architectural Separation of Concerns:** Building a pure AST parsing layer before schema binding/compilation prevents coupling SQL grammar rules with execution physical plan construction or table schemas.
  * **Deterministic Error Spans:** Hand-crafted Pratt parser ensures exact error location reporting on unexpected tokens, unclosed parentheses, missing clauses, or invalid clause ordering.
  * **Safety & Maintainability:** Hand-written recursive descent avoids heavy parser-generator dependencies, keeping the engine lightweight, 100% safe Rust, and easily auditable.

---

## Decision 31: SQL Semantic Binding, Type Coercion & Decoupled Execution Layer

* **Decision:** Build a semantic binding and translation layer (`src/sql/binder.rs`) that maps the SQL AST (`SelectStatement`) directly to Phase 14 query execution structures (`Query<'a>`) without duplicating query evaluation logic:
  1. **Decoupled Architecture:** The SQL subsystem serves exclusively as a frontend parsing and binding layer. TreeDB's Phase 14 execution engine remains the single, authoritative query execution implementation.
  2. **Name Resolution & Contextual Binding (`BindingContext`):** Resolves table names against the catalog and column identifiers (including qualified `table.column` and `alias.column`) against the table's `Schema`.
  3. **SQL Standard Precedence & Qualified Wildcards:** Implements SQL-standard precedence where `NOT` binds looser than comparisons (`NOT (x = 1)`), and supports qualified table wildcards (`table.*`, `alias.*`).
  4. **Contextual Numeric Literal Coercion:** Coerces numeric scalar literals (e.g. integer `42`) when compared against typed columns (`UInt64` or `Float64`) while preserving strict type validation across incompatible types.
  5. **Direct Engine & Transaction APIs:** Exposes `execute_sql(&str) -> Result<QueryResult>` on both `TreeDb` and `Transaction<'db>`, providing seamless transaction visibility, read-your-writes consistency, and CLI access.
* **Rationale:**
  * **Zero Duplication:** Reusing Phase 14 operators (`FilterOperator`, `ProjectOperator`, `SortOperator`, `OffsetLimitOperator`) and primary-key index optimizations ensures 100% semantic consistency between SQL queries and programmatic queries.
  * **Transaction-Aware Execution:** SQL queries executed within transactions naturally observe uncommitted mutations and participate in transaction commit/rollback lifecycles.

---

## Decision 32: Persistent Secondary B+ Tree Indexes & Composite Key Representation

* **Decision:** Implement persistent secondary B+ Tree indexes as first-class catalog-managed database objects:
  1. **Index Identity & Metadata:** Introduce monotonic, never-reused `IndexId(pub u32)` counters persisted in the catalog root page. Catalog page headers extend backward-compatibly using reserved bytes to store `next_index_id` and `index_count`.
  2. **Canonical Key Encoding (`IndexKey`):** Deterministic, order-preserving byte encoding across all supported scalar types (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`). Integer and floating point representations use big-endian sign-bit and sortable IEEE transformations ensuring lexicographical byte comparison strictly preserves mathematical ordering.
  3. **Composite Entry Ordering `(IndexKey, RecordId)`:** Secondary B+ Tree leaf nodes store sorted pairs of `(IndexKey, RecordId)`. By including `RecordId` in the composite key ordering, non-unique indexes naturally form contiguous clusters for identical index keys without requiring separate posting list structures or specialized tree layouts.
  4. **Automatic Transactional Maintenance:** `Table::insert`, `Table::update`, and `Table::delete` maintain all secondary indexes attached to the table in the same transaction context, protected by WAL physical page deltas and participating in ARIES crash recovery and CLR runtime rollbacks.
  5. **Query Access Path Selection:** Query execution prioritizes primary-key optimizations, then secondary index point lookups and range scans when eligible predicates exist on indexed columns, retrieving matching `RecordId`s and evaluating full `WHERE` predicates on the fetched tuples.
* **Rationale:**
  * **Unified Storage Architecture:** Secondary indexes leverage the existing buffer pool, WAL manager, page manager, and freelist without inventing parallel logging or caching subsystems.
  * **Zero Duplicate Code:** The `(IndexKey, RecordId)` composite design reuses balanced B+ Tree search, split, merge, and range scan mechanics directly.
  * **Strict Referential Invariants:** Dropping tables automatically cascades to drop all attached secondary indexes and returns all allocated pages to the persistent freelist.

---

## Decision 33: Relational Aggregation, GROUP BY & HAVING Execution Engine

* **Decision:** Implement aggregate functions (`COUNT(*)`, `COUNT(expr)`, `SUM`, `AVG`, `MIN`, `MAX`), `GROUP BY`, and `HAVING` as a pure, query-layer execution operator (`AggregateOperator`) without modifying the storage engine, B+ Tree, or secondary index layers:
  1. **Single-Pass Incremental Accumulators (`AggregateState`):** State transitions occur per row without buffering raw rows for each group:
     * `COUNT(*)` counts all input rows; `COUNT(expr)` ignores `NULL`s.
     * `SUM` returns `Int64`, `UInt64`, or `Float64`, ignoring `NULL` values. Empty or all-NULL inputs evaluate to `Value::Null` (not zero). Checked integer addition raises `Error::AggregateOverflow` on overflow.
     * `AVG` uses an accumulator tracking `(sum, non_null_count)` and produces `DataType::Float64` (`sum / non_null_count`). Empty or all-NULL inputs evaluate to `Value::Null`.
     * `MIN` / `MAX` support all comparable types (`Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`), ignoring `NULL`s and returning `Value::Null` on empty inputs. Float comparisons use `f64::total_cmp` for deterministic handling of `-0.0`, `NaN`, and infinities.
  2. **Deterministic Group Key (`CanonicalValue`, `GroupKey`):** Exact canonical type and value encoding mapping `Float64` using `f64::to_bits()` to avoid NaN/negative-zero collisions. Multiple `NULL` grouping keys are treated as identical, belonging to the same group.
  3. **Strict Query Execution Pipeline Ordering:**
     $$\text{Scan / Index Access} \rightarrow \text{Filter (WHERE)} \rightarrow \text{GROUP BY / Aggregation} \rightarrow \text{HAVING} \rightarrow \text{Projection} \rightarrow \text{Sort (ORDER BY)} \rightarrow \text{Pagination (Offset/Limit)}$$
  4. **Strict SQL Grouping Semantic Validation:**
     * Aggregates in `WHERE` clauses are rejected at semantic binding time (`Error::AggregateInWhere`).
     * Nested aggregates (e.g. `SUM(COUNT(*))`) are rejected by the parser (`Error::NestedAggregate`).
     * Every expression in `SELECT`, `HAVING`, and `ORDER BY` must either appear in `GROUP BY` or be derived entirely from aggregate functions (`Error::UngroupedColumn`, `Error::InvalidHavingExpression`).
     * Output column aliases are preserved in the projected schema and available for `ORDER BY`.
  5. **Global Implicit Grouping vs Grouped Execution:** Queries with aggregates but no `GROUP BY` produce exactly one row for all inputs (including empty input sets, returning e.g. `COUNT(*) = 0`, `SUM = NULL`). Grouped queries over empty input sets produce zero rows.
* **Rationale:**
  * **Strict Separation of Storage & Query:** Storage remains focused on rows, indexes, persistence, transactions, and recovery. Aggregation is purely execution-time state.
---

## Decision 34: Relational JOINs & Execution Engine

* **Decision:** Implement `INNER JOIN`, `LEFT JOIN` / `LEFT OUTER JOIN`, table aliases, and multi-table chained joins as a clean query-layer pipeline extension:
  1. **Explicit AST Representation (`JoinClause`, `JoinType`):** Joins are modeled explicitly in the AST without lowering to WHERE filters:
     ```rust
     pub enum JoinType { Inner, Left }
     pub struct JoinClause {
         pub join_type: JoinType,
         pub table: TableRef,
         pub on: SqlExpr,
     }
     ```
  2. **Multi-Table Semantic Binding & Qualification (`BindingContext`, `TableBinding`):**
     * Columns can be referenced unqualified (`id`) or qualified by table name or alias (`users.id`, `u.id`).
     * Unqualified lookups search across all participating tables. Exactly one match resolves successfully; multiple matches raise structured `Error::AmbiguousColumn(name)`; zero matches raise `Error::ColumnNotFound(name)`.
     * References to unknown table aliases raise `Error::UnknownTableAlias(alias)`.
     * Intermediate column names in joined schemas are normalized to `"{qualifier}.{column_name}"`, ensuring unique provenance throughout subsequent `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, and `SELECT` projections.
  3. **SQL 3-Valued Logic in `ON` Predicates:**
     * `ON` expressions evaluate under SQL 3VL where only `Value::Boolean(true)` is treated as a match. `Value::Boolean(false)` and `Value::Null` are non-matches.
     * `NULL = NULL` evaluates to `NULL` (falsy in join conditions), ensuring NULL foreign keys never join erroneously.
  4. **LEFT JOIN NULL-Padding & COUNT Semantics:**
     * Unmatched left-side rows are padded with `Value::Null` for all columns of the right-side schema.
     * Aggregation over left-joined tables strictly respects SQL semantics: `COUNT(*)` counts the row (evaluating to 1 for an unmatched left row), while `COUNT(right.col)` ignores NULL and evaluates to 0.
  5. **Nested-Loop Execution & Composable Operators:**
     * Execution uses `JoinOperator::apply` evaluating joins left-to-right deterministically.
     * Intermediate table results are materialized sequentially in `Transaction::execute_sql` before joining, respecting the buffer pool single-mutable-borrow rule without leaking pins.
  6. **Complete Pipeline Ordering:**
     $$\text{FROM} \rightarrow \text{JOIN(s)} \rightarrow \text{WHERE} \rightarrow \text{GROUP BY} \rightarrow \text{Aggregation} \rightarrow \text{HAVING} \rightarrow \text{Projection} \rightarrow \text{ORDER BY} \rightarrow \text{OFFSET/LIMIT}$$
* **Rationale:**
  * **Storage Independence:** JOIN execution operates purely on row streams in the query layer, leaving the underlying B+ Tree, slotted page record store, WAL, and crash recovery layers untouched.
  * **Future Extensibility:** The nested-loop operator interface is designed to permit pluggable hash joins, merge joins, or indexed nested loop joins in future phases without altering SQL binding or AST definitions.

---

## Decision 35: Rule-Based Query Optimizer & Physical Access-Path Planner

* **Decision:** Implement a deterministic, rule-based query optimizer and explicit physical access-path planning layer separating SQL semantic binding from physical execution:
  1. **Planning Boundary:**
     $$\text{SQL} \rightarrow \text{Lexer/Parser} \rightarrow \text{Binder} \rightarrow \text{BoundSelect} \rightarrow \text{Optimizer} \rightarrow \text{PhysicalPlan} \rightarrow \text{Execution}$$
  2. **Explicit Physical Access Paths (`AccessPath`):**
     * `PrimaryPointLookup { key: Value }`: Single-key B+ Tree lookup via `table.get(key)`.
     * `PrimaryRangeScan { start: Option<Value>, end: Option<Value> }`: Contiguous primary-key range scan via `table.scan(start, end)`.
     * `SecondaryIndexScan { index_id: IndexId, scan_type: SecondaryScanType }`: Direct point lookup (`Point(IndexKey)`) or range scan (`Range { start, end }`) over secondary index B+ Tree leaves, fetching matching `RecordId`s followed by row hydration via `table.get_by_record_id`.
     * `FullTableScan`: Fallback full primary B+ Tree scan over the entire table.
  3. **Deterministic Access-Path Selection Hierarchy:**
     $$\text{Primary Point Lookup} > \text{Primary Range Scan} > \text{Secondary Index Scan (ordered by IndexId ascending)} > \text{Full Table Scan}$$
     If multiple secondary indexes match candidate predicates, tie-breaking is strictly deterministic based on numerical `IndexId` order.
  4. **Predicate Decomposition & Relation Affiliation:**
     * Conjunctions (`AND`) are recursively split into constituent conjuncts.
     * Each conjunct is analyzed for relation affiliation: local single-table predicates are pushed down to the respective `PhysicalTableScan`, while multi-table or outer-referencing predicates remain at the join or outer filter level.
  5. **Residual Filter Preservation & SQL 3VL Correctness:**
     * An access path only replaces the exact predicate bounds it satisfies; any remaining or partially matched conjuncts are preserved in `PhysicalTableScan.residual_filter`.
     * Non-indexable predicates (`OR`, `NOT`, `IS NULL`, `IS NOT NULL`, complex expressions) and `NULL` comparisons (`col = NULL` evaluating to 3VL NULL) are never transformed into index lookups and are safely retained as residual or post-join filters.
  6. **LEFT JOIN Safety Invariant:**
     * `WHERE` predicates referencing the right side of a `LEFT JOIN` are preserved as post-join filters to eliminate unmatched NULL-padded rows in accordance with SQL semantics.
     * `ON` predicates referencing only the right side are safely pushed down to the right table scan before the join.
  7. **Plan Inspection (`PhysicalPlan::Display`, `EXPLAIN`):**
     * Textual representation provides inspectable, deterministic plan trees for debugging, tests, and future tooling.
---

## Decision 36: SQL DDL & DML Transactional Mutation Engine

* **Decision:** Implement complete SQL DDL (`CREATE TABLE`, `DROP TABLE`) and DML (`INSERT INTO`, `UPDATE`, `DELETE FROM`) directly integrated into the existing transactional, storage, catalog, WAL, freelist, and recovery engine without duplicating storage semantics:
  1. **Statement AST & Result Representation:**
     * Dedicated AST types: `Statement` enum (`Select`, `CreateTable`, `DropTable`, `Insert`, `Update`, `Delete`).
     * Dedicated structured execution result: `StatementResult` (`Query(QueryResult)`, `Inserted { rows }`, `Updated { rows }`, `Deleted { rows }`, `CreatedTable { table }`, `DroppedTable { table }`).
  2. **Catalog & Storage Integration for DDL:**
     * `CREATE TABLE table_name (col type [PRIMARY KEY] [NOT NULL], ...)` maps directly to `Transaction::create_table(name, schema)`.
     * `DROP TABLE table_name` maps to `Transaction::drop_table(name)` which cascades page reclamation to freelist.
     * All DDL operations participate in the active transaction with full rollback and crash-recovery support.
  3. **DML Execution & Optimizer Reuse:**
     * `INSERT INTO table [(cols...)] VALUES ((vals...), ...)` validates schema types, evaluates expressions, applies numeric literal coercion (`Int64 -> UInt64/Float64`), enforces `NOT NULL` and primary key uniqueness, updates all secondary indexes, and appends WAL records.
     * `UPDATE table SET col = expr, ... [WHERE predicate]` reuses the Phase 20 rule-based optimizer (`AccessPath` selection: `PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan`, `FullTableScan` with residual filter) to fetch candidate rows, evaluates SET expressions against candidate rows, enforces PK immutability, validates types/nullability, applies atomic secondary index maintenance, and updates record store tuples.
     * `DELETE FROM table [WHERE predicate]` similarly reuses the Phase 20 optimizer to fetch candidate rows and invokes `Table::delete` to reclaim primary keys, record store slots, overflow chains, and secondary index entries.
  4. **Strict Primary Key & Index Invariants:**
     * Direct PK mutation via UPDATE is strictly rejected with `Error::InvalidPrimaryKey`.
     * Unique secondary index collisions during INSERT or UPDATE atomically fail with `Error::DuplicateIndexKey` and rollback any partial modifications.
* **Rationale:**
  * **Unified Storage Architecture:** SQL is a transactional frontend, guaranteeing zero divergence between programmatic API operations and SQL mutations.
  * **Durability & Recovery:** All writes generate standard WAL records, automatically participating in ARIES crash recovery and checkpointing without custom recovery paths.

---

## Decision 37: SQL Subqueries, Nested Binding Scopes, Correlation, and 3VL Semantics

* **Decision:** Implement a first-class SQL subquery system supporting scalar, `IN`/`NOT IN`, `EXISTS`/`NOT EXISTS`, and correlated subqueries directly integrated into the binder, optimizer, physical planner, and transactional execution engine without creating a separate query engine:
  1. **AST Representation (`SqlExpr`):**
     * `SqlExpr::Subquery(Box<SelectStatement>)`: Scalar subqueries returning at most one row and one column.
     * `SqlExpr::Exists { subquery: Box<SelectStatement>, negated: bool }`: Row existence predicates (`EXISTS` / `NOT EXISTS`).
     * `SqlExpr::InSubquery { expr: Box<SqlExpr>, subquery: Box<SelectStatement>, negated: bool }`: Subquery membership tests (`IN` / `NOT IN`).
     * `SqlExpr::InList { expr: Box<SqlExpr>, list: Vec<SqlExpr>, negated: bool }`: Constant list membership tests.
  2. **Nested Binding Scopes & Lexical Resolution:**
     * `BindingContext` supports recursive parent links (`parent: Option<&'a BindingContext<'a>>`) and catalog access (`catalog: Option<&'a dyn TableCatalog>`).
     * Column resolution searches the innermost scope first; if unresolved, it traverses outward through enclosing scopes.
     * Outer column references are explicitly represented as `Expr::OuterColumn { name, data_type, scope_depth }` where `scope_depth >= 1`.
     * Innermost columns shadow outer columns for unqualified identifiers; qualified identifiers (`u.id`) resolve to the visible matching table/alias in that scope.
  3. **Shape & Cardinality Invariants:**
     * **Scalar Subqueries:** Must project exactly 1 column (`Error::SubqueryColumnCountMismatch`). At runtime, 0 rows evaluate to `Value::Null`, 1 row returns the scalar value, and $>1$ rows fail with `Error::ScalarSubqueryMultipleRows`.
     * **IN / NOT IN Subqueries:** Must project exactly 1 column (`Error::SubqueryColumnCountMismatch`).
     * **EXISTS / NOT EXISTS:** May project any number of columns (`SELECT *`, `SELECT 1`, `SELECT NULL, NULL`); only row presence ($>0$ vs $0$) determines truth.
  4. **SQL 3-Valued Logic (3VL) Preservation:**
     * `x IN (subquery)` evaluates to `TRUE` if any matching non-NULL value is found, `NULL` if no match exists but the subquery contains a `NULL`, and `FALSE` if no match and no `NULL`s exist.
     * `x NOT IN (subquery)` evaluates to `FALSE` if a match exists, `NULL` if no match exists but a `NULL` is present in the subquery or `x` is `NULL`, and `TRUE` if no match and no `NULL`s exist. Empty subqueries evaluate to `TRUE` under `NOT IN`.
     * `EXISTS` is strictly 2-valued (`TRUE` / `FALSE`) and is unaffected by `NULL` values in projected columns.
  5. **Rule-Based Optimizer Integration & Uncorrelated Materialization:**
     * Every subquery statement is individually bound and optimized through `Optimizer::optimize`, selecting optimal access paths (`PrimaryPointLookup`, `SecondaryIndexScan`, etc.).
     * Uncorrelated subqueries (with no outer column references) are pre-evaluated once per statement execution and materialized (`MaterializedSubqueryResult`) prior to scanning outer rows.
     * Correlated subqueries execute per outer row with an execution context stack (`outer_contexts: &[(&Row, &Schema)]`) preserving complete outer row state across arbitrary nesting depths.
  6. **Transaction Visibility & Composition:**
     * Subqueries execute within the active `Transaction` context using `TxnBufferPoolCatalog`, observing uncommitted mutations and respecting transaction rollback without introducing new WAL records for reads.
     * Subqueries compose seamlessly with JOINs, GROUP BY, aggregations, HAVING, ORDER BY, LIMIT, and DML `WHERE` clauses.
* **Rationale:**
  * **Unified Execution:** Reusing `PhysicalPlan` and `Optimizer` for subqueries avoids redundant execution engines while preserving full access-path optimization inside inner queries.
  * **Decoupled Future Decorrelation:** Preserving explicit `SubqueryExpr` and `OuterColumn` representation allows future cost-based decorrelation or subquery flattening phases without modifying the binder or AST.

---

## Decision 38: Common Table Expressions (CTEs), Scope Rules, Dependency Analysis, and Statement-Local Materialization

* **Decision:** Implement non-recursive SQL Common Table Expressions (`WITH cte_name AS (...) SELECT ...`) as first-class, statement-local logical relations fully integrated into TreeDB's existing Lexer, Parser, AST, Binder, Optimizer, and Transaction execution pipelines:
  1. **AST Representation (`WithClause` & `Cte`):**
     * `WithClause`: Encloses a sequential vector of `Cte` definitions (`ctes: Vec<Cte>`).
     * `Cte`: Contains `name: String`, optional explicit column list `columns: Option<Vec<String>>`, and the parsed query AST `query: Box<SelectStatement>`.
     * `SelectStatement`: Extended with `pub with: Option<WithClause>`.
  2. **Binding Architecture & Visibility Rules:**
     * **Sequential Left-to-Right Visibility:** A CTE is visible to subsequent sibling CTEs defined later in the same `WITH` clause and to the main query body. Forward references and self-references are rejected during binding.
     * **Physical Table Shadowing:** When a CTE name matches a persistent physical table name in the catalog, the CTE shadows the physical table for the main query and subsequent CTEs. However, the CTE body itself evaluates against the catalog and any preceding CTEs, permitting CTEs to query a physical table with the same name without accidental self-recursion.
     * **Subquery Scope Inheritance:** Subqueries within CTEs or within the main query inherit visible CTE relations via `TxnBufferPoolCatalog` and `BindingContext`.
  3. **Schema Derivation & Explicit Column Lists:**
     * If explicit column names are provided (`WITH u(uid, uname) AS (...)`), TreeDB validates that the number of explicit column names matches the projected column count (`Error::CteColumnCountMismatch`) and that no duplicate column names exist in the explicit column list (`Error::DuplicateCteColumnName`).
     * If no explicit column list is provided, column names and aliases from the CTE `SELECT` query projection are preserved.
  4. **Dependency Tracking & Cycle Detection:**
     * During semantic planning, `collect_cte_dependencies` inspects table references in base tables, joins, and nested subquery filters/projections.
     * `validate_cte_acyclicity` constructs an explicit dependency DAG across all bound CTEs (`BoundCte.dependencies`) and executes a 3-color DFS cycle detector, returning `Error::CyclicCteDependency` on any direct or indirect cycle.
  5. **Independent Optimization & Access-Path Selection:**
     * Each CTE query body is individually optimized using the Phase 20 rule-based optimizer (`Optimizer::optimize`), selecting optimal access paths (`PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan`, `FullTableScan`).
     * `explain_sql` formats CTE plans deterministically (`CTE <name>`, dependencies if any, indented sub-plan) alongside the `Main Query` plan.
  6. **Statement-Local In-Memory Materialization Model:**
     * CTEs are transient in-memory relations evaluated upon statement execution into a statement-scoped store (`HashMap<String, (Schema, Vec<Row>)>`).
     * CTE evaluation does NOT allocate disk pages, modify the persistent catalog, alter the freelist, or write WAL records.
     * CTEs execute within the active `Transaction` context, observing uncommitted in-flight mutations and respecting transaction rollback.
  7. **Deferral of Recursive CTEs (`WITH RECURSIVE`):**
     * Recursive CTE execution (`WITH RECURSIVE`) is deferred to a future dedicated phase. Recursive syntax without fixed-point iteration operators is rejected with structured errors rather than silently executing as non-recursive CTEs.
* **Rationale:**
  * **Zero Duplication:** Materializing statement-local relations in memory and reading from `cte_store` in `execute_physical_plan_internal` allows complete reuse of all downstream physical operators (`Filter`, `Project`, `Join`, `Aggregate`, `Sort`, `OffsetLimit`) without creating temporary persistent tables or a second query execution engine.
  * **Guaranteed Engine Safety:** Strict cycle detection and non-persistence ensure that complex CTE queries never cause stack overflow, catalog pollution, or disk storage leaks.














