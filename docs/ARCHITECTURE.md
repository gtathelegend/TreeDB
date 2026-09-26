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

```text
                          TreeDb
                            |
                        Transaction
                            |
                   +--------+--------+
                   |                 |
               SQL Parser       Query Engine
               (Lexer/AST)           |
                   |           +-----+-----+
               SelectStmt      |           |
                           Expr/Eval   Operators
                               |           |
                         +-----+-----+  +--+--+
                         |           |  |     |
                       Literal    Column Scan Filter ...
```

---

## 1. SQL Lexer, Parser & AST Layer (`src/sql`)

### `Lexer`, `Token`, & `SourceSpan` (`src/sql/lexer.rs`, `src/sql/token.rs`, `src/sql/span.rs`)
* **Role:** Zero-dependency lexical scanner tokenizing SQL query strings with precise source position tracking.
* **Responsibilities:**
  * **Token Stream:** Emits typed `Token` items carrying `TokenKind` (keywords, identifiers, literals, operators, delimiters, EOF) and `SourceSpan { start, end }`.
  * **Keywords & Identifiers:** Case-insensitive recognition for standard SQL keywords (`SELECT`, `FROM`, `WHERE`, `ORDER`, `BY`, `ASC`, `DESC`, `LIMIT`, `OFFSET`, `AND`, `OR`, `NOT`, `IS`, `NULL`, `TRUE`, `FALSE`, `AS`). Case-sensitive identifiers with length limit validation (`MAX_IDENTIFIER_LEN = 128`).
  * **Literals:** Supports single-quoted string literals with SQL-standard `''` escaping, signed integers, IEEE floats, boolean literals (`TRUE`/`FALSE`), and `NULL`.
  * **Comments & Whitespace:** Preserves byte offsets while skipping whitespace and single-line SQL comments (`-- ...`).

### `Parser` & Pratt Expression Parser (`src/sql/parser.rs`)
* **Role:** Recursive descent parser and Pratt precedence climbing expression parser for SQL queries, DDL, and DML statements.
* **Responsibilities:**
  * **Statements Supported:**
    * `SELECT ... FROM ... [JOIN ...]* [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT n] [OFFSET n];`
    * `CREATE TABLE table (col type [PRIMARY KEY] [NOT NULL], ...);`
    * `DROP TABLE table;`
    * `INSERT INTO table [(cols...)] VALUES ((vals...), ...);`
    * `UPDATE table SET col = expr, ... [WHERE expr];`
    * `DELETE FROM table [WHERE expr];`
  * **Join Syntax:** Supports `INNER JOIN`, shorthand `JOIN`, `LEFT JOIN`, and `LEFT OUTER JOIN` with table aliases and boolean `ON` expressions. Preserves source join order.
  * **Aggregate Expressions:** Case-insensitive parsing for `COUNT(*)`, `COUNT(expr)`, `SUM(expr)`, `AVG(expr)`, `MIN(expr)`, `MAX(expr)`. Rejects nested aggregates (e.g. `SUM(COUNT(*))`), empty aggregate calls, and wildcard arguments for non-COUNT functions (`SUM(*)`).
  * **Precedence Hierarchy:**
    1. Parenthesized expressions `(expr)`
    2. Primary literals, identifiers, and aggregate function calls
    3. Unary minus (`-`)
    4. Postfix predicates (`IS NULL`, `IS NOT NULL`)
    5. Relational comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`) with normalization of `<>` and `!=` to `SqlBinaryOp::NotEq`
    6. Logical `NOT` (SQL standard precedence binding looser than comparisons)
    7. Logical `AND`
    8. Logical `OR`
  * **Standalone Boundary:** Operates completely decoupled from runtime storage, schemas, or buffer pools via `treedb::sql::parse_sql(sql)` and `treedb::sql::parse_statement_sql(sql)`.

### `StatementResult` & Transactional Execution Engine (`src/engine/transaction.rs`)
* **Role:** Transactional execution coordinator for DDL, DML, and diagnostic introspection statements (`txn.execute_statement(sql)`).
* **Responsibilities:**
  * **Statement Routing:** Dispatches parsed `Statement` AST nodes to dedicated transactional execution routines (`execute_create_table`, `execute_drop_table`, `execute_create_index`, `execute_drop_index`, `execute_insert`, `execute_update`, `execute_delete`, `execute_select`, and `Statement::Explain`).
  * **Secondary-Index DDL:** Handles `CREATE [UNIQUE] INDEX` and `DROP INDEX` by delegating directly to `Transaction::create_secondary_index` and `Transaction::drop_secondary_index`. Scans and indexes all existing table rows on creation, enforces uniqueness (including strict single-`NULL` uniqueness), unlinks catalog metadata, and recycles all index pages via freelist on drop.
  * **Query Plan Introspection (`EXPLAIN`):** Binds and optimizes target query statements through the existing physical optimizer, returning a `StatementResult::Query` with a single `plan` string column containing the deterministic formatted physical plan without executing underlying operations or mutating persistent storage.
  * **Optimizer Reuse:** UPDATE and DELETE statements with WHERE clauses reuse the Phase 20 rule-based optimizer (`TableOptInfo`, `Optimizer::optimize`) to pick primary key lookups, primary range scans, or secondary index scans before executing table mutations.
  * **Constraint & Schema Validation:** Validates schema column types, converts numeric literals contextually (`Int64` -> `UInt64`/`Float64`), enforces `NOT NULL` constraints, rejects primary key updates, and validates unique index constraints.
  * **Result Typing:** Returns structured `StatementResult` variants (`Query`, `Inserted`, `Updated`, `Deleted`, `CreatedTable`, `DroppedTable`, `CreatedIndex`, `DroppedIndex`).
  * **WAL & Durability:** All mutations execute through existing `Table`, `Catalog`, and secondary B+ Tree APIs, ensuring complete WAL generation, ARIES recovery compatibility, and rollback on abort.



### `BindingContext`, `BoundSelect`, `TableBinding` & SQL Binder (`src/sql/binder.rs`)
* **Role:** Semantic analysis and translation layer bridging SQL ASTs with Phase 14/18/19 query execution structures.
* **Responsibilities:**
  * **Multi-Table Name Resolution:** Resolves table names against the catalog and columns (both unqualified `col` and qualified `table.col`/`alias.col`) across all joined relations via `BindingContext`.
  * **Ambiguity & Alias Checking:** Rejects ambiguous unqualified column references appearing in multiple tables (`Error::AmbiguousColumn`) and unknown table qualifiers (`Error::UnknownTableAlias`).
  * **Intermediate Join Schemas:** Normalizes column identifiers in intermediate joined schemas to `"{qualifier}.{col_name}"` to preserve provenance throughout filtering, grouping, aggregation, projection, and sorting.
  * **Wildcard Expansion:** Expands full `*` across all joined tables in left-to-right relation order, or qualified `alias.*` to all columns from that specific table schema.
  * **Semantic Grouping Validation:** Enforces SQL grouping rules: rejects aggregates in `WHERE`, validates that every expression in `SELECT`, `HAVING`, and `ORDER BY` is either present in `GROUP BY` or derived entirely from aggregate functions.
  * **Literal Coercion:** Coerces numeric literals when compared to typed columns or aggregate outputs (e.g. integer literal `42` in `COUNT(*) >= 1` coerced to `Value::UInt64`).
  * **Expression Type Inference:** Verifies data types statically on ON predicates, projections, filters, aggregates, HAVING, and sort keys before query execution.

---

## 2. Query Execution & Expression Layer (`src/query`)


### `Expr`, `AggregateExpr`, `AggregateState`, & Evaluator (`src/query/expr.rs`)
* **Role:** AST-based scalar expression and aggregate accumulator system with static type inference, validation, and SQL-compliant three-valued logic (3VL).
* **Responsibilities:**
  * **Expression AST:** `Literal(Value)`, `Column(String)`, `Binary { left, op, right }`, `Unary { op, expr }`, `IsNull(expr)`, `IsNotNull(expr)`.
  * **Aggregate Expressions:** `AggregateExpr::CountAll`, `AggregateExpr::Function { func, expr }` with `AggregateFunction` (`Count`, `Sum`, `Avg`, `Min`, `Max`).
  * **Incremental Accumulators (`AggregateState`):** State machine for single-pass aggregation (`CountAll`, `Count`, `SumInt64`, `SumUInt64`, `SumFloat64`, `Avg`, `Min`, `Max`). Returns `Value::Null` for empty/all-null input on `SUM`, `AVG`, `MIN`, `MAX`, and `0` for `COUNT`. Checked integer addition prevents silent overflow (`Error::AggregateOverflow`).
  * **Deterministic Group Keys (`CanonicalValue`, `GroupKey`):** Exact canonical type and value encoding mapping `Float64` using `f64::to_bits()` to avoid NaN/negative-zero collisions, treating all `NULL` group keys as a single group.
  * **Static Type Inference:** `Expr::infer_type(&self, schema: &Schema) -> Result<DataType>` verifies type compatibility and detects invalid operands (e.g., non-boolean in logical operators, incomparable types) before row execution.
  * **Three-Valued Logic (3VL):** Strict implementation of SQL boolean logic (`TRUE`, `FALSE`, `UNKNOWN`/`NULL`) for `AND`, `OR`, and `NOT`.
  * **Comparison Evaluation:** Strict type-safe comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`) returning `Value::Null` if either operand is `NULL`.

### `Query`, `QueryResult`, & Logical Operators (`src/query/operator.rs`, `src/query/builder.rs`)
* **Role:** Composable relational query pipeline and programmatic query builder executing over `Table<'a>` and `Transaction<'db>`.
* **Responsibilities:**
  * **Pipeline Architecture:**
    1. **Scan / Index Access:** Evaluates primary-key point lookup / range scan or secondary index lookups/scans; otherwise executes full table scans for each participating relation.
    2. **Join (`JoinOperator`):** Evaluates nested-loop joins for `INNER JOIN` and `LEFT JOIN` / `LEFT OUTER JOIN` left-to-right. Left join pads unmatched left rows with `Value::Null` for all right-relation columns. `ON` predicates evaluate under SQL 3VL where only `Value::Boolean(true)` is a match.
    3. **Filter (`FilterOperator`):** Evaluates `WHERE` predicate expressions over joined rows, retaining matching rows (`Value::Boolean(true)`).
    4. **GROUP BY & Aggregation (`AggregateOperator`):** Computes group keys for input rows and updates incremental `AggregateState`s per group. Produces 1 row per group (or exactly 1 row for global non-grouped aggregation). Respects `COUNT(*)` (counting rows) vs `COUNT(right.col)` (ignoring NULLs).
    5. **HAVING Filter (`FilterOperator`):** Evaluates `HAVING` predicates over the aggregated/grouped intermediate schema.
    6. **Projection (`ProjectOperator`):** Evaluates projection expressions, generating projected `Schema` definitions and derived output `Row`s.
    7. **Sort (`SortOperator`):** Multi-key stable sorting with configurable NULL placement and deterministic `f64::total_cmp` float ordering.
    8. **Pagination (`OffsetLimitOperator`):** In-memory offset skipping and limit truncation.
  * **Programmatic API:** Fluent builder via `TreeDb::query(name)` and `Transaction::query(name)` supporting `.filter()`, `.group_by()`, `.aggregate()`, `.having()`, `.project()`, `.sort_by()`, `.sort()`, `.offset()`, `.limit()`, and `.execute()`.

### `Optimizer`, `AccessPath`, `CostModel`, `CardinalityEstimator`, & `PhysicalPlan` (`src/query/optimizer.rs`, `src/query/cost.rs`, `src/query/cardinality.rs`, `src/query/stats.rs`)
* **Role:** Deterministic statistics-aware and cost-based query optimizer and physical execution planner producing executable `PhysicalPlan` trees from `BoundSelect`.
* **Responsibilities:**
  * **Table & Index Statistics (`src/query/stats.rs`):** `TableStatistics`, `ColumnStatistics` (row count, null count, distinct count, min/max), and `IndexStatistics` (indexed row count, distinct key count, null count). In-memory `StatsRegistry` manages advisory statistics populated via `Table::collect_statistics(&mut self)`, `db.collect_statistics()`, or SQL `ANALYZE <table>;`.
  * **Cardinality Estimation (`CardinalityEstimator`, `src/query/cardinality.rs`):** Computes estimated output row counts for full scans, equality predicates ($1/\text{NDV}$), `IS NULL` / `IS NOT NULL`, ranges via domain bounds $\frac{\text{val} - \text{min}}{\text{max} - \text{min}}$, boolean combinations (`AND`, `OR`, `NOT`), and inner equi-joins $\frac{|A| \times |B|}{\max(\text{ndv}_A, \text{ndv}_B)}$.
  * **Centralized Cost Model (`CostModel`, `src/query/cost.rs`):** Centralizes all execution cost weights (`PAGE_READ_COST`, `ROW_CPU_COST`, `FILTER_CPU_COST`, `BPLUS_TREE_HEIGHT_COST`, `SEC_INDEX_LOOKUP_COST`, `SEC_INDEX_FETCH_COST`, `PK_LOOKUP_COST`, `TUPLE_FETCH_COST`, `SORT_CPU_COST`, `JOIN_CPU_COST`, `AGG_CPU_COST`, `WINDOW_CPU_COST`) with monotonic cost formulas for scans, lookups, joins, sorting, and aggregations.
  * **Access Path Candidate Generation & Selection:** Identifies all candidate access paths (`FullTableScan`, `PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan`), computing estimated rows and cost for each candidate.
  * **Deterministic Tie-Breaking:** Selects lowest estimated cost candidate, tie-breaking by `lowest cost -> lowest estimated rows -> priority (PK point > PK range > secondary index > full scan) -> index ID -> index name`.
  * **Bounded Inner-Join Ordering:** Evaluates all $N!$ join permutations for 2–5 inner-joined relations with statistics, choosing the permutation with the lowest cumulative cost, tie-breaking to the original syntactic order. Preserves `LEFT JOIN` outer ordering strictly.
  * **Predicate Decomposition & Pushdown:** Splits `AND` conjunctions into individual conjuncts. Pushes table-local predicates down into individual `PhysicalTableScan` access paths and residual filters before executing joins.
  * **LEFT JOIN Outer Safety:** Restricts pushdown across `LEFT JOIN` boundaries; filters on the right side in `WHERE` are retained post-join to ensure unmatched rows are correctly filtered out.
  * **Residual Filter Preservation:** Always retains unconsumed predicate components in `PhysicalTableScan.residual_filter` to ensure full semantic equivalence.
  * **Deterministic Plan Inspection:** Exposes `cost: {:.2}, estimated_rows: {:.0}` in `PhysicalPlan::Display` and `EXPLAIN <query>;` with strict zero-execution and zero-persistence guarantees.

---

## 1. Engine, Catalog & Relational Layer (`src/engine`)

### `Table`, `Schema`, `Column`, `DataType`, & `Row` (`src/engine/types.rs`, `src/engine/table.rs`)
* **Role:** High-level typed relational abstraction layer mapping logical rows to deterministic binary tuples stored across the B+ Tree index and slotted RecordStore.
* **Responsibilities:**
  * **Logical Type System:** `DataType` (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`) with explicit 1-byte discriminants and typed scalar `Value` variants.
  * **Relational Schema Definition:** `Schema { columns: Vec<Column>, primary_key_idx: usize }` with `Column { name, data_type, nullable }`. Enforces single-column primary keys (`UInt64` or `Int64`), non-nullable PK constraints, unique column names, and length limits.
  * **Canonical Primary Key Mapping:** Preserves signed integer monotonicity by mapping `Int64(i)` to `(i as u64) ^ (1 << 63)` for direct, order-preserving B+ Tree indexing where negative keys naturally precede positive keys.
  * **Deterministic Binary Tuple Encoding:** Format version 1 packing column count, a compact null bitmap (`ceil(N / 8)` bytes, 1 = NULL), little-endian fixed-width primitives (`bool`, `i64`, `u64`, `f64`), and 4-byte length-prefixed variable strings and raw bytes.
  * **Relational Table Handle:** `Table<'a>` provides full CRUD (`insert`, `get`, `update`, `delete`, `scan`) operating transparently within active transactions, supporting multi-page overflow rows via RecordStore, and automatically keeping catalog page root metadata synchronized upon B+ Tree splits/merges.

### `Catalog`, `CatalogPage`, `TableMetadata`, & `TableId` (`src/engine/catalog.rs`)
* **Role:** Persistent schema and table metadata management layer decoupling logical object definitions from physical B+ Tree and RecordStore structures.
* **Responsibilities:**
  * **Stable Table Identifiers:** `TableId(pub u32)` allocated monotonically and never reused upon table drop.
  * **Persistent Table Metadata:** `TableMetadata { table_id, name, schema_version, root_page_id, record_page_id, flags, schema }`.
  * **Paged Chaining Structure:** Anchored on Page 1 (`DEFAULT_METADATA_PAGE_ID`) with `PageType::Catalog = 7` (or `PageType::Metadata`), dynamically chaining additional catalog pages via `next_catalog_page_id` to support unbounded table capacity without artificial ceilings.
  * **Transactional Lifecycle:** Table creation (`create_table`) and dropping (`drop_table`) integrate seamlessly into the active transaction context (`TxnId`), writing physical `PageDelta` WAL records, supporting runtime rollback with CLRs, and recovering via ARIES.
  * **Complete Freelist Reclamation:** Dropping a table reclaims all associated B+ Tree pages, record pages, and overflow chains back into the persistent freelist with zero page leaks.
  * **Structural & Referential Validation:** `validate_catalog()` verifies magic (`CATL`), version, table count, ID monotonicity, name syntax, schema invariants, and referential validity of root and record page pointers.

### `TreeDb`
* **Role:** High-level unified database interface coordinating the catalog, relational tables, index, record storage, buffer pool, and transactions.
* **Responsibilities:**
  * Key-value operations: `put(key, value)`, `update(key, value)`, `get(key)`, `delete(key)`, `scan(start, end)`.
  * Relational table operations: `create_table(name, schema)`, `open_table(name)`, `drop_table(name)`, `get_table(name)`, `get_table_by_id(id)`, `list_tables()`, `validate_catalog()`.
  * Transaction coordination: `begin() -> Result<Transaction<'_>>`, `begin_txn()`, `commit_txn(txn_id)`, `abort_txn(txn_id)`.
  * Automatic ARIES crash recovery upon opening existing database and WAL files.
  * Explicit buffer pool and WAL synchronization via `sync()`.
  * Synchronous checkpointing and WAL prefix recycling (`checkpoint()`, `checkpoint_and_truncate_wal()`).

### `Transaction` & `TxnState`
* **Role:** Explicit atomic transaction abstraction coordinating multi-operation units of work under a single WAL transaction identifier (`TxnId`).
* **Responsibilities:**
  * **State Machine:** Governed by explicit `TxnState` (`Active`, `Committed`, `Aborted`). Mutations on closed transactions return structured errors (`TransactionAlreadyCommitted`, `TransactionAlreadyAborted`).
  * **Single-Transaction Exclusivity:** Enforces single-threaded mutual exclusion on `TreeDb` via Rust's mutable borrowing and dynamic tracking (`Error::TransactionActive`).
  * **Read-Your-Writes:** Immediate visibility of uncommitted inserts, updates, deletes, and catalog modifications for subsequent in-transaction reads.
  * **Commit Durability:** Commits all mutations atomically, ensuring WAL commit record is durably flushed before acknowledging completion.
  * **Runtime Rollback with CLRs:** On explicit `abort()`, walks the transaction's `PageDelta` history backward in reverse chronological order via `prev_lsn`, applies `before_image` byte slices, appends Compensation Log Records (CLRs), and flushes the WAL.
  * **Deterministic Drop Behavior:** Uncommitted transactions dropped out of scope automatically invoke rollback to maintain memory/disk cleanliness.

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

## 5. Secondary Index Engine (`src/index/secondary.rs`)

```text
Table
 ├── Primary B+ Tree  (PK -> RecordId)
 ├── Secondary Index A (IndexKey -> RecordId)
 ├── Secondary Index B (IndexKey -> RecordId)
 └── RecordStore       (RecordId -> Tuple Bytes)
```

### `SecondaryIndex`, `SecondaryLeafNode` & `SecondaryInternalNode`
* **Role:** Persistent disk-backed B+ Tree implementation managing secondary access paths over `(IndexKey, RecordId)` tuples.
* **Responsibilities:**
  * **Canonical Key Encoding (`IndexKey`):** Deterministic, order-preserving binary encoding for all supported types (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`). Integer and floating point representations use big-endian sign-bit / IEEE transformations ensuring byte-level comparison strictly matches numerical ordering.
  * **Composite Key Ordering:** Entries in leaf nodes are sorted by `(IndexKey, RecordId)`. This provides deterministic contiguous clustering for non-unique index keys while guaranteeing that every entry is unique in the tree.
  * **Lifecycle & Auto-Maintenance:** Mutations via `Table::insert`, `Table::update`, and `Table::delete` automatically propagate changes to all attached secondary indexes within the active transaction.
  * **Unique Constraint Validation:** Unique secondary indexes reject duplicate insertions before allocating heap storage (`Error::DuplicateIndexKey`).
  * **Access Path Selection:** `Query::execute` detects indexed predicates in `WHERE` clauses and uses secondary index point lookups or range scans to fetch candidate `RecordId`s, evaluating full predicates on retrieved records.
  * **Cascading Reclamation:** Dropping a table or index reclaims all associated B+ Tree pages directly into the persistent freelist.

---

## 6. SQL, Optimizer & Subquery Engine (`src/sql`, `src/query`)

```text
SQL Statement
     ↓
Lexer & Parser (AST)
     ↓
Binder & Nested Scopes (BoundSelect)
     ↓
Rule-Based Optimizer (AccessPath Selection)
     ↓
Physical Plan Tree (PhysicalPlan + SubqueryPlan)
     ↓
Transactional Execution (Scan, Joins, Aggregation, 3VL Filters, Subqueries)
```

### Components
* **Lexer & Parser:** Case-insensitive keyword tokenization (`IN`, `EXISTS`, `SELECT`, `FROM`, `WHERE`, `JOIN`, `CREATE`, `DROP`, `INSERT`, `UPDATE`, `DELETE`), recursive descent parsing with operator precedence.
* **Semantic Binder & Scopes:** Resolves table and column bindings. Supports recursive parent contexts (`parent: Option<&BindingContext>`) for nested subqueries, resolving columns from innermost to outer scopes and producing `Expr::OuterColumn { name, data_type, scope_depth }`.
* **Rule-Based Optimizer:** Deconstructs conjunctions, pushes single-table predicates down to access paths (`PrimaryPointLookup`, `PrimaryRangeScan`, `SecondaryIndexScan`, `FullTableScan`), preserves residual filters, and maintains safe post-join placement for `LEFT JOIN` filters.
* **Subquery Execution:**
  * **Uncorrelated Subqueries:** Pre-evaluated once per statement execution and materialized (`MaterializedSubqueryResult`) prior to scanning outer rows.
  * **Correlated Subqueries:** Executed per outer row with an execution context stack (`outer_contexts: &[(&Row, &Schema)]`) preserving complete outer row state across arbitrary nesting depths.
  * **3-Valued Logic (3VL):** Comprehensive 3VL support for `IN`, `NOT IN`, scalar comparisons, and `EXISTS` predicates.
* **Common Table Expressions (CTEs):**
  * **AST & Grammar:** `WithClause` containing sequentially defined `Cte` nodes with optional explicit column lists (`WITH cte(col1, col2) AS (SELECT ...)`).
  * **Scope & Visibility:** Strict left-to-right sequential visibility; later CTEs can reference earlier CTEs. Forward references and self-references are rejected.
  * **Table Shadowing:** CTEs take precedence over physical tables of the same name in the main query scope without shadowing within the CTE body itself.
  * **Dependency DAG & Acyclicity:** Tracks dependencies across base tables, joins, and nested subqueries; 3-color DFS detector enforces acyclicity (`Error::CyclicCteDependency`).
  * **Materialization & Non-Persistence:** Statement-local in-memory relation storage (`cte_store`) evaluated on query invocation; zero page allocations, zero catalog modifications, zero freelist changes, and zero WAL records for read queries.
  * **Engine Integration:** Full composition with `INNER JOIN`, `LEFT JOIN`, `GROUP BY`, `HAVING`, scalar/IN/EXISTS subqueries, correlated subqueries, and deterministic `explain_sql` formatting.
* **SQL Window Functions:**
  * **AST Representation:** `WindowFunction` (`RowNumber`, `Rank`, `DenseRank`, `Lag`, `Lead`, `FirstValue`, `LastValue`), `WindowSpec` (`partition_by`, `order_by`, `frame`), `WindowFrame` with `WindowFrameBound` endpoints, and `SqlExpr::Window`.
  * **Dedicated Physical Operator (`WindowOperator`):** Evaluates between aggregation/HAVING and final query `ORDER BY`/projection, preserving input row cardinality ($N \to N$).
  * **Partitioning & Deterministic Tie-Breaking:** Partitions evaluated via `CanonicalValue` (`NULL = NULL` for partition membership); intra-partition ordering uses SQL 3VL with stable row-index tie-breaking.
  * **Framing Semantics & Defaults:** Standard SQL defaults (`ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` with `ORDER BY`; `ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING` without `ORDER BY`) and validated `ROWS` frames.
  * **Semantic Validation:** Structured errors rejecting window functions in `WHERE`, `GROUP BY`, `HAVING`, `JOIN ON`, and nested window functions.
  * **Statement-Local Execution:** Non-persistent, zero-WAL, in-memory execution fully integrated with CTEs, joins, aggregations, and transaction visibility.
* **Recursive Common Table Expressions (WITH RECURSIVE):**
  * **AST Representation & Grammar:** `WithClause { recursive: bool, ctes: Vec<Cte> }`, `CteQuery::Recursive { anchor, recursive }`, requiring explicit `UNION ALL` separation between anchor and recursive member branches.
  * **Explicit Delta Iteration Model:** Evaluates via working-set delta loop (`working_rows = anchor_rows` -> `recursive_plan(working_rows)` -> `working_rows = next_rows`) appending rows to `accumulated_rows`.
  * **Hard Execution Bounds:** Protected against runaway recursion via configurable iteration limit (`DEFAULT_MAX_RECURSION_ITERATIONS = 1000`, `Error::RecursiveCteIterationLimit`) and row count limit (`DEFAULT_MAX_RECURSION_ROWS = 1_000_000`, `Error::RecursiveCteRowLimit`).
  * **Scope & Shadowing Rules:** Recursive CTE name is visible only within the recursive member query, shadowing catalog tables of the same name. Self-reference in anchor is rejected (`Error::RecursiveCteAnchorSelfReference`). Mutual recursion (`a -> b -> a`) is statically rejected (`Error::MutualRecursion`).
  * **Schema Validation:** Enforces exact column count matching and data type compatibility between anchor and recursive member projections.
  * **Relational Integration:** Seamlessly composes with joins (`INNER`/`LEFT`), filters, aliases, preceding CTEs, subqueries, and outer query `ORDER BY`, `LIMIT`, and window functions.
* **SQL Set Operations:**
  * **AST & Precedence:** `SetOperator` (`Union`, `UnionAll`, `Intersect`, `IntersectAll`, `Except`, `ExceptAll`), `SetQueryBody` (recursive enum), `SetQuery` (with top-level WITH, ORDER BY, LIMIT, OFFSET). `INTERSECT` binds tighter (precedence 2) than `UNION`/`EXCEPT` (precedence 1). Parentheses explicitly override precedence.
  * **Physical Set Operator (`PhysicalSetOp`):** Recursive evaluation of left and right physical plan operands applying set and multiset algebra in memory.
  * **Distinctness & Equivalence:** Evaluates row identity using `CanonicalValue` comparison where `NULL = NULL`.
  * **Multiset Semantics:** `UNION ALL` preserves duplicates; `INTERSECT ALL` takes multiset minimum $\min(L, R)$; `EXCEPT ALL` subtracts multiplicities $\max(0, L - R)$.
  * **Schema Coercion & Derivation:** Output schema column names and primary attributes are determined by the left operand; column counts must match (`Error::SetOperationColumnCountMismatch`) and incompatible types are rejected (`Error::SetOperationTypeMismatch`).
  * **Full Engine Composition:** Integrated with CTEs (non-recursive and recursive), joins, aggregations, window functions, scalar/IN/EXISTS subqueries, and deterministic `explain_sql` formatting.

---

## 7. Critical Invariants

* **Write-Ahead Invariant:** `append WAL record -> WAL durable -> page modification -> page flush`. A dirty page with `page_lsn > wal.flushed_lsn` cannot reach disk without its WAL records first being flushed.
* **PageLSN Semantics:** Every page modification stamps the page with its corresponding LSN. During Redo:
  * `page.lsn() < log_record.lsn()` => Redo applied.
  * `page.lsn() >= log_record.lsn()` => Redo skipped.
  PageLSN never regresses.
* **CLR Durability Invariant:** `append CLR -> flush WAL through CLR LSN -> write recovered page`. Recovery-generated modifications are never durable before their CLR is durable on disk.
* **Engine Atomicity Boundary:** `TreeDb::put` and `TreeDb::delete` operations wrap both RecordStore heap modifications (inline and overflow) and B+ Tree index mutations in a single transaction (`TxnId`). If an uncommitted operation crashes, both index and record mutations are undone atomically.
* **Secondary Index Invariant:** Every committed table row must have exactly the corresponding secondary-index entries required by its indexed columns, and every secondary-index entry must resolve to a valid current `RecordId`.
* **Subquery Cardinality Invariant:** Scalar subqueries must return $\le 1$ row (0 rows -> `NULL`, 1 row -> value, $>1$ rows -> `Error::ScalarSubqueryMultipleRows`) and exactly 1 column.
* **CTE Non-Persistence Invariant:** Executing CTE and recursive CTE queries never modifies persistent database pages, freelist state, catalog metadata, or WAL logs.
* **Set Operation Invariant:** Executing set operations is purely in-memory, deterministic, and preserves the left operand's column names and unified types without persistent side effects.
* **Recursive Boundedness Invariant:** Recursive CTE execution never uses unbounded call stacks and deterministically terminates when the working set is empty or upon reaching configured iteration/row safety thresholds.
* **Window Cardinality Invariant:** Window function evaluation never collapses or filters rows; the output row cardinality of `WindowOperator` strictly equals input row cardinality.
* **WAL Corruption Policy:** Valid log prefixes are recovered. Incomplete trailing crash tails are safely ignored. Corruptions in the middle of durable log records return `Error::WalCorruption`.
* **Zero Pin Leaks:** All buffer pool frames must have `pin_count == 0` after every normal operation, failed recovery, successful recovery, or repeated recovery pass.
* **Recovery Restartability & Idempotence:** Recovery can be safely interrupted at any point (before/during Redo, during Undo, after CLRs) and restarted. Repeated recovery passes yield identical database states.
* **Page Reachability & Reservation Invariant:** Page 0 (Header) and Page 1 (Metadata) are strictly reserved (`is_reserved_page(page_id) == true`) and can never be freed or recycled. Every page ID in the database is either reachable via index/metadata/heap or tracked in the persistent freelist.

---

## 8. Intentionally Deferred Items

The following capabilities are intentionally deferred to subsequent phases:
* Multi-column composite secondary indexes.
* Index-only covering scans.
* Full cost-based optimizer and cardinality statistics.
* DDL statements (`CREATE INDEX`, `DROP INDEX`) in SQL grammar.
* Recursive CTE `UNION DISTINCT`, `SEARCH`, `CYCLE` clauses, and mutual recursion.
* Derived tables (`FROM (SELECT ...)`).
* Subquery flattening and cost-based decorrelation.
* Multi-threading, concurrency control, and MVCC.



