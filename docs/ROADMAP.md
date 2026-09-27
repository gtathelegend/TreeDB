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

## Phase 20: Rule-Based Optimizer & Access-Path Planner (Completed)
* [x] Decoupled logical/physical planning boundary (`BoundSelect` -> `Optimizer` -> `PhysicalPlan` -> execution)
* [x] Explicit physical access paths: `PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan` (`PointLookup`, `RangeScan`), `FullTableScan`
* [x] Rule-based predicate decomposition (`split_conjunction`, `recombine_conjunction`, `unqualify_expr`)
* [x] Deterministic access-path selection priority (PK point -> PK range -> Secondary index by IndexId asc -> Full scan)
* [x] Residual filter preservation (never discarding unconsumed predicates)
* [x] SQL 3VL NULL safety: `WHERE col = NULL` and `IS NULL` are not incorrectly matched as index point lookups
* [x] JOIN predicate pushdown: table-local filters pushed down to respective table scans before joins
* [x] LEFT JOIN safety: WHERE predicates on right table of LEFT JOIN remain post-join to eliminate NULL-padded rows; ON predicates on right table safely pushed down
* [x] Full composition: optimized scans feed seamlessly into `JoinOperator -> FilterOperator -> AggregateOperator -> SortOperator -> ProjectOperator -> OffsetLimitOperator`
* [x] Deterministic plan inspection API: `PhysicalPlan::Display`, `txn.plan_sql()`, `txn.explain_sql()`, `db.plan_sql()`, `db.explain_sql()`
* [x] Dynamic index lifecycle: immediate recognition of created/dropped secondary indexes in active transactions
* [x] Baseline vs optimized execution equivalence tests and randomized reference oracle
* [x] 20 dedicated optimizer integration tests in `tests/optimizer.rs` (358 total test suite)

---

## Phase 21: SQL DDL & DML (Completed)
* [x] Top-level statement AST: `Statement` enum (`Select`, `CreateTable`, `DropTable`, `Insert`, `Update`, `Delete`)
* [x] SQL Lexer & Parser support for `CREATE TABLE`, `DROP TABLE`, `INSERT INTO ... VALUES`, `UPDATE ... SET`, `DELETE FROM`
* [x] Column definitions, data type mappings (`BIGINT`, `UBIGINT`, `DOUBLE`, `TEXT`, `BLOB`, `BOOLEAN`), single column `PRIMARY KEY` and `NOT NULL` constraints
* [x] Transactional DDL execution: `CREATE TABLE` and `DROP TABLE` participating in active transactions with rollback and crash recovery
* [x] INSERT execution with explicit column lists and positional tuples, multi-row batches, and numeric literal coercion
* [x] UPDATE execution with multi-assignment, WHERE predicate filtering, and all-rows updates
* [x] DELETE execution with WHERE predicate filtering and all-rows updates
* [x] Rule-based optimizer reuse for UPDATE and DELETE access paths (`PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan`, `FullTableScan` with residual filters)
* [x] Atomic secondary index maintenance on INSERT, UPDATE, and DELETE with unique index constraint validation
* [x] Primary key immutability protection on UPDATE (`Error::InvalidPrimaryKey`)
* [x] Structured result API: `StatementResult` (`Query`, `Inserted`, `Updated`, `Deleted`, `CreatedTable`, `DroppedTable`)
* [x] SQL vs programmatic mutation equivalence tests and randomized reference oracle
* [x] 16 dedicated integration tests in `tests/sql_ddl_dml.rs` and 7 new parser tests in `tests/sql.rs` (381 total test suite)

---

## Phase 22: SQL Subqueries & Correlated Subqueries (Completed)
* [x] Subquery AST representation: `SqlExpr::Subquery`, `SqlExpr::Exists { subquery, negated }`, `SqlExpr::InSubquery { expr, subquery, negated }`, `SqlExpr::InList { expr, list, negated }`
* [x] SQL Lexer & Parser keyword support for `IN` and `EXISTS` with case-insensitive tokenization and operator precedence
* [x] Nested binding scopes (`BindingContext` parent chaining) with lexical scope resolution from innermost to outer scopes
* [x] Structured correlation representation (`Expr::OuterColumn { name, data_type, scope_depth }`)
* [x] Lexical shadowing: innermost table columns shadow outer enclosing scope columns unless explicitly qualified (`u.id`)
* [x] Scalar subquery cardinality enforcement (0 rows -> `NULL`, 1 row -> value, >1 rows -> `Error::ScalarSubqueryMultipleRows`)
* [x] Single-column projection shape enforcement for scalar and IN subqueries (`Error::SubqueryColumnCountMismatch`)
* [x] Multi-column and NULL projection support for `EXISTS` and `NOT EXISTS` predicates
* [x] SQL 3-valued logic (3VL) preservation for `IN` and `NOT IN` predicates with NULL values and empty subqueries
* [x] Optimizer integration: inner subqueries fully bound and optimized using Phase 20 rule-based optimizer
* [x] Uncorrelated subquery optimization: pre-evaluated once per statement execution and materialized (`MaterializedSubqueryResult`)
* [x] Correlated subquery execution: executed per outer row with parent row contexts (`outer_contexts`) supporting arbitrary correlation depth
* [x] Full composition with JOINs, GROUP BY, aggregation, HAVING, ORDER BY, LIMIT, and DML (`UPDATE`, `DELETE`)
* [x] Transaction visibility: subqueries observe uncommitted writes within the active transaction and respect rollback/abort
* [x] 24 dedicated integration tests in `tests/subquery.rs` and 2 new parser tests in `tests/sql.rs` including randomized 3VL reference oracle (405 total test suite)

---

## Phase 23: Common Table Expressions (CTEs) (Completed)
* [x] Lexer & Parser support for `WITH` keyword, case-insensitivity, single CTE, multiple comma-separated CTEs, and explicit column lists `cte(col1, col2)`
* [x] Structured AST representation: `WithClause` and `Cte` integrated into `SelectStatement` (`pub with: Option<WithClause>`)
* [x] Scoped CTE binding architecture: left-to-right sequential visibility, sibling reference, forward-reference and self-reference rejection
* [x] Schema derivation: preserving projection aliases and validating explicit column lists (`CteColumnCountMismatch`, `DuplicateCteColumnName`)
* [x] Physical table shadowing: CTEs shadow catalog tables with identical names in main queries while allowing CTE body to query base physical table
* [x] Dependency DAG analysis & cycle detection: `collect_cte_dependencies` and 3-color DFS `validate_cte_acyclicity` emitting `Error::CyclicCteDependency`
* [x] Optimizer integration: CTE bodies independently bound and optimized through Phase 20 rule-based optimizer (`PhysicalPlan.ctes`)
* [x] Explain output: deterministic hierarchical `explain_sql` formatting showing CTE plans and dependencies alongside main query plan
* [x] Statement-local in-memory materialization: executed into transient `cte_store` without persistent pages, freelist allocation, catalog mutation, or WAL writes
* [x] Full query engine composition: CTEs in `INNER JOIN`, `LEFT JOIN`, `GROUP BY`, `HAVING`, scalar/IN/EXISTS subqueries, and correlated subqueries
* [x] Transaction visibility: CTEs observe uncommitted mutations in active transactions and respect transaction rollback
* [x] 21 dedicated integration tests in `tests/cte.rs` and 2 new parser tests in `tests/sql.rs` including randomized differential reference oracle (428 total test suite)

---

## Phase 24: SQL Window Functions (Completed)
* [x] Explicit AST representation: `WindowFunction`, `WindowSpec`, `WindowFrame`, `WindowFrameBound`, `SqlExpr::Window`
* [x] Parser support for `OVER (...)`, `PARTITION BY`, `ORDER BY`, `ROWS BETWEEN ... AND ...`
* [x] Ranking functions: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()` with deterministic tie-breaking on peer rows
* [x] Navigation functions: `LAG(expr [, offset [, default]])`, `LEAD(expr [, offset [, default]])` with non-negative constant offset validation
* [x] Value functions: `FIRST_VALUE(expr)`, `LAST_VALUE(expr)` evaluated across defined window frames
* [x] Framing semantics: standard default frames (`ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` when `ORDER BY` present; `ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING` when absent) and explicit `ROWS` ranges
* [x] Dedicated physical execution operator (`WindowOperator`) executing after aggregation/HAVING and before query ORDER BY/projections, preserving input cardinality ($N \to N$)
* [x] Semantic validation: structured rejections for window functions in `WHERE`, `GROUP BY`, `HAVING`, `JOIN ON`, and nested window functions
* [x] Integration with CTEs, subqueries, joins, aggregations, aliases, final `ORDER BY`, and `LIMIT`/`OFFSET`
* [x] Optimizer integration with deterministic `explain_sql` output detailing functions, partitions, orders, and frame bounds
* [x] Statement-local in-memory evaluation model: read-only, zero persistent page allocation, zero catalog mutation, zero WAL writes
* [x] Transaction visibility and crash recovery safety
* [x] 17 dedicated integration tests in `tests/window.rs` including randomized reference oracle comparing against an independent evaluator (445 total test suite)

---

## Phase 25: Recursive Common Table Expressions (WITH RECURSIVE) (Completed)
* [x] Lexer & Parser support for `WITH RECURSIVE`, `UNION ALL`, arithmetic expressions (`+`, `-`, `*`, `/`), and case-insensitivity
* [x] Structured AST representation: `WithClause.recursive` flag and `CteQuery::Recursive { anchor, recursive }`
* [x] Scoped recursive CTE binding: anchor cannot reference recursive relation (`RecursiveCteAnchorSelfReference`); recursive member references working relation with precedence over physical tables
* [x] Schema compatibility & derivation: validates matching column counts and data types between anchor and recursive member branches (`RecursiveCteColumnCountMismatch`, `RecursiveCteSchemaMismatch`)
* [x] Explicit recursive CTE column list support and arity/uniqueness validation
* [x] Explicit delta/working-set iterative evaluation loop model with statement-local memory management
* [x] Configurable termination limits and structured error safeguards: `DEFAULT_MAX_RECURSION_ITERATIONS = 1000` (`RecursiveCteIterationLimit`) and `DEFAULT_MAX_RECURSION_ROWS = 1_000_000` (`RecursiveCteRowLimit`)
* [x] `UNION ALL` duplicate preservation semantics
* [x] Cycle detection and mutual recursion rejection: static detection across CTE definitions (`MutualRecursion`)
* [x] Relational composition: recursive CTEs in `INNER JOIN`, `LEFT JOIN`, filters, aliases, preceding CTE references, subqueries, final `ORDER BY`, `LIMIT`, and window functions
* [x] Semantic validation: structured rejection of unsupported recursive aggregation and window functions in recursive member
* [x] Optimizer integration with deterministic `explain_sql` output displaying anchor, recursive member, iteration limit, and row limit
* [x] Transaction visibility: recursive CTEs observe uncommitted mutations in active transactions and respect transaction rollback
* [x] Statement-local in-memory evaluation model: read-only, zero persistent page allocation, zero catalog mutation, zero WAL writes
* [x] 20 dedicated integration tests in `tests/recursive_cte.rs` and 1 new parser test in `tests/sql.rs` including randomized reference evaluator oracle (467 total test suite)

---

## Phase 26: SQL Set Operations (Completed)
* [x] Lexer & Parser support for `UNION`, `UNION ALL`, `INTERSECT`, `INTERSECT ALL`, `EXCEPT`, and `EXCEPT ALL`
* [x] Operator precedence (`INTERSECT` > `UNION`/`EXCEPT`), left-associativity, and explicit parentheses nesting
* [x] Structured AST representation: `SetOperator`, `SetQueryBody`, `SetQuery`, `Statement::SetQuery`, `CteQuery::SetOperation`, and subquery variants (`SqlExpr::SetSubquery`, `SqlExpr::SetExists`, `SqlExpr::InSetSubquery`)
* [x] Semantic binding & validation: column count checking (`Error::SetOperationColumnCountMismatch`), numeric type coercion, incompatible type rejection (`Error::SetOperationTypeMismatch`), and left-operand output schema derivation
* [x] Row identity equality semantics with distinct `NULL = NULL` representation (`CanonicalValue`)
* [x] Multiset arithmetic and deduplication algorithms: `UNION` (distinct), `UNION ALL` (multiset sum), `INTERSECT` (distinct), `INTERSECT ALL` ($\min$), `EXCEPT` (distinct), `EXCEPT ALL` ($\max(0, count_L - count_R)$)
* [x] Composable physical execution operator (`PhysicalSetOp`) evaluated recursively
* [x] Seamless composition with non-recursive CTEs, recursive CTEs (preserving Phase 25 recursive engine invariants), joins, aggregations, window functions, and subqueries
* [x] Top-level `ORDER BY`, `LIMIT`, and `OFFSET` execution over set operation result sets
* [x] Optimizer integration with deterministic `explain_sql` output formatting
* [x] Statement-local in-memory evaluation model: read-only, zero persistent page allocation, zero catalog mutation, zero WAL writes
* [x] Transaction visibility: set operations observe uncommitted mutations in active transactions and respect transaction rollback
* [x] 24 dedicated integration tests in `tests/set_operations.rs` including randomized reference evaluator oracle (491 total test suite)

---

## Phase 27: SQL Secondary-Index DDL (Completed)
* [x] Lexer & Parser support for `CREATE INDEX`, `CREATE UNIQUE INDEX`, and `DROP INDEX` with case-insensitive keywords and case-sensitive identifiers
* [x] Structured AST representation: `CreateIndexStatement { name, table_name, column_name, unique }` and `DropIndexStatement { name }` integrated into `Statement` enum
* [x] Grammar invariants: strict single-column restriction `(column_name)`; structured syntax errors rejecting multi-column, expression, partial, covering, and concurrent index syntax
* [x] Semantic binding & validation: `bind_create_index` resolving table/column against catalog, rejecting unknown relations (`Error::TableNotFound`), unknown columns (`Error::ColumnNotFound`), duplicate index names (`Error::DuplicateIndex`), and unsupported data types
* [x] `bind_drop_index` resolving index globally through catalog (`Error::IndexNotFound`)
* [x] Storage engine delegation: directly delegates execution to `Transaction::create_secondary_index` and `Transaction::drop_secondary_index` without duplicating the B+ Tree engine
* [x] Existing row population: `CREATE INDEX` scans and indexes all existing table rows into the persistent secondary B+ Tree
* [x] Unique index semantics: strict single-`NULL` uniqueness rule preserved from Phase 17 (`Error::UniqueIndexViolation` on duplicate values or multiple `NULL`s)
* [x] Supported index key types: canonical `IndexKey` encodings for `Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`
* [x] Page recycling & cleanup: `DROP INDEX` unlinks catalog metadata and recycles all index pages via the persistent freelist
* [x] Table mutation maintenance: subsequent `INSERT`, `UPDATE`, `DELETE` automatically maintain SQL-created secondary indexes via existing table mutation hooks
* [x] Optimizer integration: query access path planner selects `SecondaryIndexScan` on matching equality/range predicates; `DROP INDEX` immediately reverts plans to `FullTableScan`; deterministic `explain_sql` visibility
* [x] Transaction atomicity: index creation and drop participate in transactions; full rollback on `abort()` with zero leaked pages and pristine catalog state
* [x] WAL & ARIES crash recovery: index DDL operations participate in WAL logging, checkpoints, and crash recovery across restarts
* [x] Persistence: full catalog validation and index usability verified across database close and reopen
* [x] 19 dedicated integration tests in `tests/index_ddl.rs` including randomized reference oracle comparing against an in-memory BTreeMap (510 total test suite)

---

## Phase 28: SQL EXPLAIN & Query Plan Introspection (Completed)
* [x] Lexer & Parser support for `EXPLAIN <query>;` with case-insensitive `EXPLAIN` keyword
* [x] Structured AST representation: `ExplainStatement { query: Box<Statement> }` integrated into `Statement::Explain`
* [x] Statement target restriction: structured rejection of non-query statements (`EXPLAIN INSERT`, `EXPLAIN UPDATE`, `EXPLAIN DELETE`, `EXPLAIN CREATE TABLE`, `EXPLAIN CREATE INDEX`, `EXPLAIN DROP TABLE`, `EXPLAIN DROP INDEX`) via `Error::UnsupportedExplainTarget`
* [x] Binder & Optimizer integration: binds target query through identical semantic compilation pipeline and generates optimized `PhysicalPlan`
* [x] Comprehensive physical plan coverage: deterministic formatting for `TableScan` (FullTableScan, PrimaryPointLookup, PrimaryRangeScan, SecondaryIndexScan), `Filter`, `Project`, `Sort`, `LimitOffset`, `NestedLoopJoin` (Inner, Left), `Aggregate`, `Having`, `Window`, `Subquery` (Scalar, In, Exists, Correlated), `CTE`, `Recursive CTE`, and `Set Operation` (all 6 operations)
* [x] Strict no-execution invariant: plans queries without evaluating row expressions or scanning tables
* [x] Strict no-persistence mutation: zero disk page allocations, zero WAL record generation, and zero transaction state changes
* [x] Standard `QueryResult` format: returns single textual column `plan` with DataType::String containing the plan representation
* [x] Transaction & catalog visibility: reflects active uncommitted transactional metadata and reverts upon rollback
* [x] CLI integration: execution supported via `treedb query <path> "EXPLAIN <query>"`
* [x] 18 dedicated integration tests in `tests/explain.rs` covering parser, access paths, joins, aggregates, windows, subqueries, CTEs, recursive CTEs, set operations, index lifecycle, rollback, determinism, and non-mutation (528 total test suite)

---

## Phase 29: Query Statistics & Cost-Based Optimization (Completed)
* [x] Statistics abstraction & data models: `TableStatistics`, `ColumnStatistics` (row count, null count, distinct count, min/max), `IndexStatistics` (indexed row count, distinct key count, null count), `StatisticsProvider` trait, `StatsRegistry`
* [x] Statistics collection mechanism: non-destructive `Table::collect_statistics(&mut self)`, `db.collect_statistics("table")`, `txn.collect_statistics("table")`, and SQL statement `ANALYZE <table>;`
* [x] Statistics freshness policy: advisory in-memory cache, explicit collection via `collect_statistics` / `ANALYZE`, safe missing-statistics fallback to deterministic defaults
* [x] Deterministic cardinality estimation (`CardinalityEstimator`):
  * Full table scan: `stats.row_count` (fallback 1000.0)
  * Equality predicate: `1 / distinct_count` (fallback 0.1)
  * IS NULL / IS NOT NULL: `null_count / row_count` and `1 - null_fraction`
  * Range predicates: value domain fraction `(val - min) / (max - min)` with fallback `1/3`
  * Logical operators: independent multiplication for `AND`, inclusion-exclusion for `OR`, complement for `NOT`
  * Inner equi-joins: `|A| * |B| / max(ndv(A.key), ndv(B.key))`
* [x] Centralized cost model (`CostModel`): canonical weights (`PAGE_READ_COST`, `ROW_CPU_COST`, `FILTER_CPU_COST`, `BPLUS_TREE_HEIGHT_COST`, `SEC_INDEX_LOOKUP_COST`, `SEC_INDEX_FETCH_COST`, `PK_LOOKUP_COST`, `TUPLE_FETCH_COST`, `SORT_CPU_COST`, `JOIN_CPU_COST`, `AGG_CPU_COST`, `WINDOW_CPU_COST`) and deterministic `PlanCost` comparison
* [x] Access-path candidate generation & selection: evaluates `FullTableScan`, `PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan` (PointLookup, RangeScan); deterministic cost tie-breaking (cost -> estimated rows -> priority -> index ID -> index name -> full scan)
* [x] Bounded cost-based inner-join ordering: evaluates all $N!$ permutations for 2–5 relations, preserves identity ordering on ties, strictly preserves outer join (LEFT JOIN) ordering
* [x] EXPLAIN integration: formats `cost: <f64>` and `estimated_rows: <f64>` across table scans and joins while preserving zero-execution and zero-persistence invariants
* [x] Missing-statistics fallback: unanalyzed tables fall back gracefully to default rule-based priorities without changing query results
* [x] CLI integration: supports `ANALYZE <table>;` execution in REPL/CLI
* [x] 11 dedicated integration tests in `tests/statistics_optimizer.rs` including randomized differential query oracle comparing analyzed vs unanalyzed execution (539 total test suite)

---

## Phase 30: Query Execution Metrics & Runtime Plan Diagnostics (Completed)
* [x] Dedicated metrics abstraction: `OperatorMetrics`, `OperatorCounters`, `QueryExecutionMetrics`
* [x] Operator runtime instrumentation across all physical operators (`TableScan`, `Filter`, `Project`, `Sort`, `LimitOffset`, `NestedLoopJoin`, `Aggregate`, `Window`, `Subquery`, `Cte`, `RecursiveCte`, `SetOperation`)
* [x] Domain-specific counters: input rows, output rows, tuples fetched, pages read, index lookups, filter rows evaluated/passed/rejected, join candidate pairs/matches, aggregation group counts
* [x] Cardinality comparison: estimated vs. actual output rows, `estimate_error_ratio` calculation
* [x] Diagnostic wall-clock execution timing (`std::time::Instant` / `std::time::Duration`)
* [x] SQL `EXPLAIN ANALYZE <query>;` grammar, AST (`ExplainStatement { query, analyze: true }`), parser, execution, and diagnostic plan formatting
* [x] Explicit API: `txn.execute_sql_with_metrics(sql)` and `db.execute_sql_with_metrics(sql)` returning `(QueryResult, QueryExecutionMetrics)`
* [x] Transaction semantics & visibility: `EXPLAIN ANALYZE` executes inside the active transaction, observing uncommitted mutations and respecting rollback
* [x] Read-only safety: zero persistent page allocations, zero WAL record generation for read queries
* [x] Unsupported DML policy: structured rejection of non-query statements in `EXPLAIN ANALYZE` via `Error::UnsupportedExplainTarget`
* [x] 17 dedicated integration tests in `tests/explain_analyze.rs` including randomized differential query oracle (556 total test suite)

---

## Phase 31: Prepared Statements & Parameter Binding (Completed)
* [x] Positional SQL parameter syntax `?` (1-indexed, deterministic left-to-right lexical order)
* [x] AST & Bound representations: `SqlExpr::Parameter(usize)` and `Expr::Parameter { index, value }`
* [x] `PreparedStatement` abstraction and `ParameterMetadata` (`index`, `inferred_type: Option<DataType>`)
* [x] Contextual parameter type inference (`infer_statement_parameters`) during prepare with ambiguous type fallback
* [x] Strict validation: parameter count matching (`Error::ParameterCountMismatch`) and runtime type validation (`Error::ParameterTypeMismatch`)
* [x] SQL 3-Valued Logic (3VL) semantics for NULL parameters (`WHERE col = ?` with NULL evaluates to False)
* [x] Parameterized value-sensitive cost-based optimization: access-path extraction (PK point/range, secondary index scans) and selectivity estimation
* [x] Full statement coverage: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, JOINs, Aggregates/HAVING, Window functions, Set operations, Subqueries, CTEs, and Recursive CTEs
* [x] Parameter rejection in DDL and identifier positions (`CREATE TABLE ?`, `SELECT * FROM ?`)
* [x] Prepared diagnostics: `explain_prepared`, `explain_prepared_with_metrics`, `execute_prepared_with_metrics`
* [x] 12 dedicated integration tests in `tests/prepared_statements.rs` (568 total passing test suite)

---

## Phase 32: Physical Plan Cache & Prepared-Statement Reuse (Completed)
* [x] Dedicated plan cache subsystem (`PlanCache`, `PlanCacheKey`, `PlanValidity`, `PlanCacheEntry`, `PlanCacheStats`)
* [x] Strongly typed cache key: normalized SQL string, canonical parameter planning signature (`ParamPlanningSignature` using `CanonicalValue`), monotonic catalog generation, index generation, and statistics generation
* [x] Monotonic generation tracking on `TreeDb`: `catalog_generation`, `index_generation`, `statistics_generation`
* [x] Multi-level explicit cache invalidation: `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`, `ANALYZE`, `set_statistics`, `clear_statistics`
* [x] Transaction safety & DDL isolation: uncommitted catalog modifications (`has_catalog_mutations`) bypass global cache; generation bumps on commit/abort prevent uncommitted plan leakage
* [x] Parameter-sensitive planning: distinguishable cache keys for differing parameter values, preventing plan regression across parameter-dependent access paths
* [x] Bounded capacity & deterministic LRU eviction (`DEFAULT_PLAN_CACHE_CAPACITY = 128`)
* [x] Cache diagnostics: `PlanCacheStats` exposing `hits`, `misses`, `evictions`, `entries`, `capacity` on `TreeDb` and `Transaction`
* [x] Execution metrics & EXPLAIN ANALYZE integration: `plan_cache_hit: bool` tracked in `QueryExecutionMetrics`
* [x] Strict runtime-only memory residence: zero WAL records, zero disk persistence, fresh empty cache upon open/reopen
* [x] Correctness invariant: 100% semantic equivalence between cached and uncached executions
* [x] 12 dedicated integration tests in `tests/plan_cache.rs` including randomized differential reference oracle (580 total passing test suite)

---

## Phase 33: External Sort & Spill-to-Disk Execution (Completed)
* [x] Dedicated external sorting subsystem (`ExternalSorter`, `SortConfig`, `TempRunFile`, `RunReader`, `SortExecutionMetrics`)
* [x] Dual-path execution: in-memory fast path for sub-budget inputs, spilling to disk for super-budget inputs
* [x] Deterministic binary run file format (`TEMP_RUN_MAGIC = b"TSRN"`, little-endian encoded tagged rows `(sequence, Row)`)
* [x] Full ORDER BY semantics preservation: `ASC`/`DESC`, `NULLS FIRST`/`LAST`, multi-key, all scalar types, `f64::total_cmp` float/NaN ordering
* [x] Stable tie-breaking via monotonic sequence numbers matching in-memory sort order
* [x] Bounded K-way merge using min-heap priority queue (`BinaryHeap<MergeItem>`)
* [x] Multi-pass merge support for large numbers of runs exceeding `max_temp_files`
* [x] RAII-managed temporary file lifecycle with automatic deletion on `Drop`
* [x] Exclusion from WAL, transaction commit/rollback, and `.treedb` database pages
* [x] Resource safeguards: `max_temp_bytes` (`Error::ExternalSortQuotaExceeded`) and `max_merge_passes` (`Error::ExternalSortMaxPassesExceeded`)
* [x] Execution metrics & `EXPLAIN ANALYZE` integration reporting `runs_created`, `bytes_spilled`, `merge_passes`, `peak_memory_bytes`
* [x] 12 dedicated integration tests in `tests/external_sort.rs` including randomized differential reference oracle (592 total passing test suite)

---

## Phase 34: External Aggregation & Spill-to-Disk GROUP BY (Completed)
* [x] Dedicated external aggregation subsystem (`ExternalAggregator`, `AggregateConfig`, `TempAggregateFile`, `AggregateExecutionMetrics`)
* [x] Dual-path execution: in-memory fast path for sub-budget inputs, partitioned spill-to-disk for super-budget inputs
* [x] Deterministic partition file binary format (`TEMP_AGG_MAGIC = b"TAGR"`, little-endian encoded group values and typed `AggregateState` records)
* [x] Complete SQL aggregation semantics preservation: `COUNT(*)`, `COUNT(expr)`, `SUM`, `AVG`, `MIN`, `MAX`, NULL keys, Float64 NaN canonicalization
* [x] Algebraic intermediate state merging via `AggregateState::merge_from` with checked overflow detection
* [x] Multi-pass recursive partitioning with salted hash function (`hash(key, pass)`) when partition reloads exceed budget
* [x] Atomic single-group handling avoiding infinite recursion on high-multiplicity keys
* [x] RAII-managed temporary partition file lifecycle with guaranteed automatic cleanup on `Drop`
* [x] Complete isolation from WAL, transaction commit/rollback, and `.treedb` persistent database pages
* [x] Resource safeguards: `max_temp_bytes` (`Error::ExternalAggregateQuotaExceeded`) and `max_merge_passes` (`Error::ExternalAggregateMaxPassesExceeded`)
* [x] Execution metrics & `EXPLAIN ANALYZE` integration reporting `group_count`, `spill_partitions`, `runs_created`, `bytes_spilled`, `merge_passes`, `peak_memory_bytes`
* [x] 12 dedicated integration tests in `tests/external_aggregation.rs` including randomized differential reference oracle (604 total passing test suite)

---

## Phase 35: External Hash Join & Spill-to-Disk JOIN (Completed)
* [x] Dedicated external hash join subsystem (`ExternalHashJoiner`, `JoinConfig`, `TempJoinFile`, `JoinExecutionMetrics`)
* [x] Dual-path execution: in-memory fast path for sub-budget relations, partitioned spill-to-disk for super-budget relations
* [x] Deterministic partition file binary format (`TEMP_JOIN_MAGIC = b"TJRN"`, 16-byte header, little-endian encoded `Row` records)
* [x] Equi-join condition analysis & deconstruction (`analyze_equi_join`, `EquiJoinPlan`) with residual predicate evaluation
* [x] Complete SQL join semantics preservation: `INNER JOIN`, `LEFT JOIN`, composite keys, duplicate keys ($N \times M$), and NULL key isolation
* [x] SQL 3-Valued Logic: `NULL = NULL` is UNKNOWN and never matches; left NULL keys are emitted with right NULL-padding in LEFT JOIN
* [x] Multi-pass recursive sub-partitioning with salted hash function (`hash_join_key(key, pass)`) and duplicate key skew accommodation
* [x] RAII-managed temporary partition file lifecycle with guaranteed automatic cleanup on `Drop`
* [x] Complete isolation from WAL, transaction commit/rollback, and `.treedb` persistent database pages
* [x] Resource safeguards: `max_temp_bytes` (`Error::ExternalJoinQuotaExceeded`) and `max_partition_passes` (`Error::ExternalJoinMaxPassesExceeded`)
* [x] Execution metrics & `EXPLAIN ANALYZE` integration reporting `rows_left`, `rows_right`, `candidate_pairs`, `matched_rows`, `spill_partitions`, `runs_created`, `bytes_spilled`, `partition_passes`, `peak_memory_bytes`
* [x] 12 dedicated integration tests in `tests/external_hash_join.rs` including randomized differential reference oracle (616 total passing test suite)

---

## Phase 36: Advanced Query Processing & Optimizer Enhancements (Future)
* [ ] Merge join physical execution operator
* [ ] Index-only covering scans
* [ ] Multi-column composite secondary indexes
* [ ] Expression indexes & partial indexes
* [ ] Derived tables (`FROM (SELECT ...)`)
* [ ] Subquery flattening and decorrelation
* [ ] Interactive CLI commands (`create-table`, `list-tables`, `stats`)

















