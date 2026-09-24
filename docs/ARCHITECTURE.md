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
* **Role:** Recursive descent parser and Pratt precedence climbing expression parser for the scoped SQL `SELECT` dialect.
* **Responsibilities:**
  * **Clause Grammar:** Enforces strict clause order: `SELECT [wildcard | expressions [AS alias]] FROM table [WHERE expr] [ORDER BY expr [ASC | DESC], ...] [LIMIT n] [OFFSET n];`.
  * **Precedence Hierarchy:**
    1. Parenthesized expressions `(expr)`
    2. Primary literals and identifiers
    3. Unary minus (`-`)
    4. Postfix predicates (`IS NULL`, `IS NOT NULL`)
    5. Relational comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`) with normalization of `<>` and `!=` to `SqlBinaryOp::NotEq`
    6. Logical `NOT` (SQL standard precedence binding looser than comparisons)
    7. Logical `AND`
    8. Logical `OR`
  * **Standalone Boundary:** Operates completely decoupled from runtime storage, schemas, or buffer pools via `treedb::sql::parse(sql)`.

### `BindingContext`, `BoundSelect` & SQL Binder (`src/sql/binder.rs`)
* **Role:** Semantic analysis and translation layer bridging SQL ASTs with Phase 14 query execution structures.
* **Responsibilities:**
  * **Name Resolution:** Resolves table names against the catalog and columns (both unqualified `name` and qualified `table.name`/`alias.name`) against table schemas.
  * **Wildcard Expansion:** Expands unqualified `*` and qualified `alias.*` to all columns from the target `Schema`.
  * **Literal Coercion:** Coerces numeric literals when compared to typed columns (e.g. integer literal `42` to `Value::UInt64` or `Value::Float64`).
  * **Expression Type Inference:** Verifies data types statically on projections, filters, and sort keys before query execution.
  * **Zero Execution Duplication:** Translates `SelectStatement` directly into `BoundSelect` and Phase 14 `Expr`s, delegating physical execution entirely to `Query<'a>`.

---

## 2. Query Execution & Expression Layer (`src/query`)


### `Expr`, `BinaryOp`, `UnaryOp`, & Evaluator (`src/query/expr.rs`)
* **Role:** AST-based scalar expression system with static type inference, validation, and SQL-compliant three-valued logic (3VL).
* **Responsibilities:**
  * **Expression AST:** `Literal(Value)`, `Column(String)`, `Binary { left, op, right }`, `Unary { op, expr }`, `IsNull(expr)`, `IsNotNull(expr)`.
  * **Static Type Inference:** `Expr::infer_type(&self, schema: &Schema) -> Result<DataType>` verifies type compatibility and detects invalid operands (e.g., non-boolean in logical operators, incomparable types) before row execution.
  * **Three-Valued Logic (3VL):** Strict implementation of SQL boolean logic (`TRUE`, `FALSE`, `UNKNOWN`/`NULL`) for `AND`, `OR`, and `NOT`.
  * **Comparison Evaluation:** Strict type-safe comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`) returning `Value::Null` if either operand is `NULL`. IEEE-754 `NaN` comparisons evaluate predictably to `false`.

### `Query`, `QueryResult`, & Logical Operators (`src/query/operator.rs`, `src/query/builder.rs`)
* **Role:** Composable relational query pipeline and programmatic query builder executing over `Table<'a>` and `Transaction<'db>`.
* **Responsibilities:**
  * **Pipeline Architecture:**
    1. **Scan:** Evaluates primary-key point lookup (`pk == lit`) or range scan (`pk >= low AND pk < high`) optimizations; otherwise executes a full B+ Tree scan.
    2. **Filter (`FilterOperator`):** Evaluates predicate expressions, retaining rows where `predicate == Value::Boolean(true)` (discarding `false` and `Null`).
    3. **Sort (`SortOperator`):** Multi-key stable sorting with configurable NULL placement (defaulting to NULLs first in `ASC`, NULLs last in `DESC`) and deterministic `f64::total_cmp` float ordering.
    4. **Pagination (`OffsetLimitOperator`):** In-memory offset skipping and limit truncation.
    5. **Projection (`ProjectOperator`):** Evaluates projection expressions, generating projected `Schema` definitions and derived output `Row`s.
  * **Programmatic API:** Fluent builder via `TreeDb::query(name)` and `Transaction::query(name)` supporting `.filter()`, `.project()`, `.sort_by()`, `.sort()`, `.offset()`, `.limit()`, and `.execute()`.


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

## 6. Critical Invariants

* **Write-Ahead Invariant:** `append WAL record -> WAL durable -> page modification -> page flush`. A dirty page with `page_lsn > wal.flushed_lsn` cannot reach disk without its WAL records first being flushed.
* **PageLSN Semantics:** Every page modification stamps the page with its corresponding LSN. During Redo:
  * `page.lsn() < log_record.lsn()` => Redo applied.
  * `page.lsn() >= log_record.lsn()` => Redo skipped.
  PageLSN never regresses.
* **CLR Durability Invariant:** `append CLR -> flush WAL through CLR LSN -> write recovered page`. Recovery-generated modifications are never durable before their CLR is durable on disk.
* **Engine Atomicity Boundary:** `TreeDb::put` and `TreeDb::delete` operations wrap both RecordStore heap modifications (inline and overflow) and B+ Tree index mutations in a single transaction (`TxnId`). If an uncommitted operation crashes, both index and record mutations are undone atomically.
* **Secondary Index Invariant:** Every committed table row must have exactly the corresponding secondary-index entries required by its indexed columns, and every secondary-index entry must resolve to a valid current `RecordId`.
* **WAL Corruption Policy:** Valid log prefixes are recovered. Incomplete trailing crash tails are safely ignored. Corruptions in the middle of durable log records return `Error::WalCorruption`.
* **Zero Pin Leaks:** All buffer pool frames must have `pin_count == 0` after every normal operation, failed recovery, successful recovery, or repeated recovery pass.
* **Recovery Restartability & Idempotence:** Recovery can be safely interrupted at any point (before/during Redo, during Undo, after CLRs) and restarted. Repeated recovery passes yield identical database states.
* **Page Reachability & Reservation Invariant:** Page 0 (Header) and Page 1 (Metadata) are strictly reserved (`is_reserved_page(page_id) == true`) and can never be freed or recycled. Every page ID in the database is either reachable via index/metadata/heap or tracked in the persistent freelist.

---

## 7. Intentionally Deferred Items

The following capabilities are intentionally deferred to subsequent phases:
* Multi-column composite secondary indexes.
* Index-only covering scans.
* Full cost-based optimizer and cardinality statistics.
* DDL statements (`CREATE INDEX`, `DROP INDEX`) in SQL grammar.
* Multi-threading, concurrency control, and MVCC.

