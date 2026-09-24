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

## Phase 11: Transaction Manager & Atomic Multi-Operation Transactions (Completed)
* [x] Explicit `Transaction<'db>` abstraction with state machine (`Active`, `Committed`, `Aborted`)
* [x] Single WAL transaction context (`TxnId`) mapping across multiple CRUD operations (`put`, `update`, `delete`, `get`, `scan`)
* [x] Read-your-writes consistency within an active transaction
* [x] Strict WAL commit durability ordering (`append commit -> flush WAL through commit LSN`)
* [x] Comprehensive runtime rollback with Compensation Log Records (CLRs) on explicit `abort()`
* [x] Deterministic automatic rollback on `Drop` for uncommitted active transactions
* [x] Complete isolation from concurrent transactions (single-threaded engine discipline)
* [x] Multi-key atomicity and same-key repeated mutation handling
* [x] Interaction across checkpoints, WAL prefix truncation, page reuse, and multi-page overflow records
* [x] Deterministic randomized transaction oracle comparison vs in-memory `BTreeMap`
* [x] 15 dedicated transaction integration tests (193 total test suite)

## Phase 12: Persistent Catalog & Schema Metadata (Completed)
* [x] Stable logical table identifiers (`TableId(pub u32)`) allocated monotonically without reuse on drop
* [x] Persistent table metadata structure (`TableMetadata { table_id, name, schema_version, root_page_id, record_page_id, flags }`)
* [x] Dedicated catalog page layout (`PageType::Catalog = 7` / `PageType::Metadata`) with 32-byte header (`CATL` magic, version 1, `next_table_id`, `table_count`, `next_catalog_page_id`)
* [x] Paged catalog chaining via `next_catalog_page_id` supporting unbounded table capacity
* [x] Table name validation rules (UTF-8, non-empty, max 64 bytes, case-sensitive exact uniqueness)
* [x] Transactional table creation (`create_table`) and deletion (`drop_table`) in `Transaction` and `TreeDb`
* [x] Complete freelist reclamation: dropping a table frees its B+ Tree nodes, record pages, and overflow chains with zero page leaks
* [x] Comprehensive catalog integrity validator (`validate_catalog`) verifying magic, version, IDs, name syntax, and referential page bounds
* [x] Full ARIES crash-recovery integration (uncommitted create/drop rollback, committed create/drop replay)
* [x] Checkpoint and WAL prefix truncation integration preserving catalog mutation history
* [x] Deterministic randomized catalog oracle comparison vs in-memory `BTreeMap`
* [x] 22 dedicated catalog integration tests (215 total test suite)

---

## Phase 13: Typed Tables & Relational Tuple Storage (Completed)
* [x] Rich logical type system (`DataType::Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`) with typed `Value` scalar representation
* [x] Relational schema representation (`Column { name, data_type, nullable }`, `Schema { columns, primary_key_idx }`)
* [x] Single-column primary key constraints (`UInt64` or `Int64`, non-nullable)
* [x] Order-preserving canonical primary key mapping for signed integers: `(i as u64) ^ (1 << 63)`
* [x] Deterministic binary tuple encoding format version 1 (column count, null bitmap, packed fixed-width primitives, 4-byte length-prefixed strings and raw bytes)
* [x] Schema validation for columns, primary keys, and incoming row values
* [x] High-level relational table handle (`Table<'a>`) supporting CRUD (`insert`, `get`, `update`, `delete`, `scan`)
* [x] Dynamic catalog root metadata synchronization on tree root splits and merges
* [x] Multi-row and multi-table atomic transaction integration (commit / rollback)
* [x] Full ARIES crash recovery and checkpoint integration
* [x] Multi-page overflow support for large text strings and binary blobs (> 4070 bytes)
* [x] Deterministic randomized table oracle comparison vs in-memory `BTreeMap`
* [x] 22 dedicated table integration tests (237 total test suite)

---

## Phase 14: Query Execution & Expression Engine (Completed)
* [x] AST-based scalar expression system (`Expr::Literal`, `Expr::Column`, `Expr::Binary`, `Expr::Unary`, `Expr::IsNull`, `Expr::IsNotNull`)
* [x] Binary operators (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`) and unary negation (`NOT`)
* [x] Static type inference and operand compatibility validation (`Expr::infer_type(&self, schema) -> Result<DataType>`)
* [x] SQL-compliant three-valued logic (3VL) for `AND`, `OR`, `NOT` (`TRUE`, `FALSE`, `UNKNOWN`/`NULL`)
* [x] Three-valued comparison semantics returning `Value::Null` on NULL operands
* [x] Logical query result representation (`QueryResult { schema: Schema, rows: Vec<Row> }`)
* [x] Composable logical operators:
  * [x] Scan operator with B+ Tree primary key order
  * [x] Filter operator (`FilterOperator`) evaluating predicates and retaining only `Value::Boolean(true)`
  * [x] Project operator (`ProjectOperator`) evaluating expression lists and deriving output schemas
  * [x] Sort operator (`SortOperator`) with multi-key sorting, explicit NULL placement (NULLs first in ASC, NULLs last in DESC), and `f64::total_cmp` float ordering
  * [x] Pagination operator (`OffsetLimitOperator`) for OFFSET and LIMIT
* [x] Fluent programmatic query builder (`Query<'a>`) on `TreeDb::query()` and `Transaction::query()`
* [x] Primary key optimizations:
  * [x] Point lookup optimization (`pk == lit`) using direct B+ Tree point lookup
  * [x] Range scan optimization (`pk >= low AND pk < high`) using B+ Tree range scan
  * [x] 100% equivalence verified between optimized lookups and full table scans
* [x] Multi-table transactional query execution with uncommitted mutation visibility and rollback safety
* [x] Deterministic randomized expression evaluator oracle testing vs independent 3VL reference
* [x] Deterministic randomized query pipeline oracle testing vs pure in-memory `Vec<Row>` model
* [x] 21 dedicated query integration tests (258 total test suite)

---

## Phase 15: SQL Lexer, Parser & AST (Completed)
* [x] Scoped SQL lexer (`Lexer`) tokenizing keywords, identifiers, literals, operators, and punctuation
* [x] Case-insensitive keyword recognition and case-sensitive identifier support (up to 128 bytes)
* [x] Source span tracking (`SourceSpan { start, end }`) for precise syntax error reporting
* [x] Literals: single-quoted strings with `''` escaping, integers, floats, booleans (`TRUE`/`FALSE`), and `NULL`
* [x] Single-line comments (`-- comment`) and whitespace preservation
* [x] Precedence climbing / Pratt expression parser (`SqlExpr`) supporting:
  * [x] Parenthesized expressions `(expr)`
  * [x] Unary operators (`NOT`, `-`)
  * [x] Postfix `IS NULL` and `IS NOT NULL`
  * [x] Comparisons (`=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`) with inequality normalization to `SqlBinaryOp::NotEq`
  * [x] Logical conjunctions: `AND` > `OR`
* [x] Complete AST representations: `SelectStatement`, `SelectItem`, `TableRef`, `SqlExpr`, `SqlLiteral`, `SqlBinaryOp`, `SqlUnaryOp`, `OrderDirection`, `OrderByExpr`
* [x] Scoped `SELECT` grammar: `SELECT [wildcard | expressions [AS alias]] FROM table [WHERE expr] [ORDER BY expr [ASC | DESC], ...] [LIMIT n] [OFFSET n];`
* [x] Explicit clause order enforcement and trailing token rejection
* [x] Standalone parsing API: `treedb::sql::parse(sql)` and `treedb::sql::tokenize(sql)` decoupled from engine storage and catalogs
* [x] 19 dedicated SQL parser tests (277 total test suite)

---

## Phase 16: SQL Binder & Execution Engine (Completed)
* [x] Semantic binding layer (`src/sql/binder.rs`) translating AST `SelectStatement` to `BoundSelect`
* [x] Table resolution against catalog and structured `TableNotFound` errors
* [x] Column resolution against schema with `ColumnNotFound` errors and qualified identifier support (`table.column`, `alias.column`)
* [x] Wildcard expansion (`*` and `table.*`/`alias.*`) to table schema column definitions
* [x] Contextual literal coercion for `UInt64` and `Float64` columns
* [x] Output column alias preservation (`AS alias`) throughout projection operators and query schema derivation
* [x] Static type inference and validation on projections, filters, and sort keys
* [x] Direct SQL execution APIs: `Transaction::execute_sql` and `TreeDb::execute_sql`
* [x] Full transaction integration: uncommitted mutation visibility, read-your-writes, and rollback safety
* [x] Preservation of primary-key point lookup and range scan index optimizations
* [x] Reusable CLI command: `treedb query <path> "<SQL>"`
* [x] 14 dedicated SQL execution integration tests (292 total test suite)

---

## Phase 17: Persistent Secondary Indexes (Completed)
* [x] Monotonic, stable, persistent `IndexId`s independent from `TableId`
* [x] Persistent catalog extension with `IndexMetadata` and backward-compatible page header serialization
* [x] Canonical order-preserving index key encoding (`IndexKey`) supporting `Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`
* [x] Composite entry representation `(IndexKey, RecordId)` for non-unique secondary indexes with deterministic traversal
* [x] Unique secondary index constraints (`DuplicateIndexKey`) and transactional rollback on duplicate rejection
* [x] Programmatic APIs: `txn.create_index(...)`, `txn.drop_index(...)`, `txn.index_lookup(...)`, `txn.index_scan(...)`
* [x] Automatic secondary index maintenance on `Table::insert`, `Table::update`, `Table::delete`
* [x] Cascading table drop reclaiming all secondary index pages into the freelist
* [x] Structural catalog and index tree validators (`validate_catalog`, `validate_index`, `validate_indexes`)
* [x] Query engine integration: automatic secondary index selection on equality and range predicates
* [x] Transaction, WAL/ARIES crash recovery, and checkpoint/WAL truncation integration
* [x] 16 dedicated secondary index tests including randomized reference model oracle (308 total test suite)

---

## Phase 18: Aggregations, GROUP BY & HAVING (Completed)
* [x] Aggregation functions (`COUNT(*)`, `COUNT(expr)`, `SUM`, `AVG`, `MIN`, `MAX`)
* [x] Incremental accumulator state machine (`AggregateState`) with integer overflow protection (`Error::AggregateOverflow`)
* [x] Deterministic in-memory canonical group keys (`GroupKey`, `CanonicalValue`) with float bitwise comparison and unified NULL group semantics
* [x] Composable `AggregateOperator` supporting both grouped aggregation and 1-group global aggregation
* [x] SQL Lexer & Parser support for `GROUP BY` and `HAVING` clauses and case-insensitive aggregate function calls
* [x] Parser rejection of nested aggregates (`SUM(COUNT(*))`), empty calls (`COUNT()`), and wildcards in non-COUNT functions (`SUM(*)`)
* [x] Semantic validation in SQL binder: aggregate rejection in `WHERE`, ungrouped column validation in `SELECT`/`HAVING`/`ORDER BY`
* [x] Contextual numeric literal type coercion for HAVING comparisons (e.g. `COUNT(*) >= 1`)
* [x] Query execution pipeline integration: `Scan/Index -> Filter -> GROUP BY/Aggregate -> HAVING -> Project -> Sort -> Limit/Offset`
* [x] Programmatic `Query` builder API extensions: `.group_by(...)`, `.aggregate(...)`, `.having(...)`
* [x] Transaction visibility, crash recovery, and database reopen durability integration
* [x] 13 dedicated aggregation tests including SQL vs programmatic equivalence and randomized reference oracle (325 total test suite)

---

## Phase 19: Relational JOINs (Completed)
* [x] SQL Lexer and Parser support for `INNER JOIN`, `JOIN`, `LEFT JOIN`, `LEFT OUTER JOIN`
* [x] Explicit AST representation: `JoinType` (`Inner`, `Left`), `JoinClause`, table aliases (`FROM table AS alias`, `FROM table alias`), and chained join clauses
* [x] Multi-table semantic binding (`BindingContext`, `TableBinding`) with qualified (`table.col`, `alias.col`) and unqualified column resolution
* [x] Structured ambiguity detection (`Error::AmbiguousColumn`) and unknown alias error reporting (`Error::UnknownTableAlias`)
* [x] Wildcard expansion for full joins (`*`) in deterministic left-to-right table order and qualified wildcards (`alias.*`)
* [x] Composable query execution operator: `JoinOperator::apply` implementing nested-loop join strategy with SQL 3-valued logic (3VL) `ON` predicate evaluation
* [x] LEFT JOIN NULL-padding across all right-side relation columns on unmatched left rows
* [x] SQL 3VL `NULL` key semantics: `NULL = NULL` does not match in standard equality joins
* [x] Strict pipeline composition: `FROM -> JOIN(s) -> WHERE -> GROUP BY -> Aggregation -> HAVING -> Projection -> ORDER BY -> OFFSET/LIMIT`
* [x] Aggregation integration: proper handling of `COUNT(*)` (counting NULL-padded rows) vs `COUNT(right.col)` (evaluating to 0 on NULLs)
* [x] Transaction visibility, read-your-writes, rollback on abort, and durability across crash recovery
* [x] 11 dedicated integration tests in `tests/join.rs` and 2 dedicated parser tests in `tests/sql.rs` including randomized reference oracle (338 total test suite)

---

## Phase 20: Advanced Query Processing & DDL (Future)
* [ ] Hash join and merge join execution operators
* [ ] SQL `CREATE INDEX` and `DROP INDEX` DDL statements
* [ ] Index-only covering scans
* [ ] Multi-column composite secondary indexes
* [ ] Subqueries and Common Table Expressions (CTEs)
* [ ] Cost-based join optimization and cardinality estimation
* [ ] Interactive CLI commands (`create-table`, `list-tables`, `stats`)









