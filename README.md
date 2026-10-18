# TreeDB

> **TreeDB — A persistent storage engine built from first principles in Rust.**

TreeDB is a single-node, disk-backed storage engine designed to demonstrate and explore database internals through a clean, modern, and production-disciplined implementation.

It is built for educational, research, and deep-systems exploration. It is not intended to compete with commercial production database engines, but rather to clearly demonstrate how storage engines operate under the hood.

---

## High-Level Architecture

```text
Application / CLI
        │
        ▼
   TreeDB API (Implemented)
        │
        ▼
     B+ Tree (Index: Key → RID)
        │
        ▼
   Record Store (Heap: RID → Bytes)
        │
        ▼
   Buffer Pool (Page Cache & Frame Table)
        │
   ┌────┴────────────────────────┐
   │ (Write-Ahead Rule)          │
   ▼                             ▼
WAL Manager (Implemented)    Page Manager (Physical I/O)
   │                             │
   ▼                             ▼
WAL File (.treedb.wal)       Database File (.treedb)
```

---

## Core Capabilities & Status

| Subsystem / Feature | Status | Description |
| :--- | :--- | :--- |
| **Database File & Header** | **Implemented** | 4096-byte pages, format verification, magic bytes `TREEDB01` (Version 2) |
| **Page Manager** | **Implemented** | Page allocation, random-access I/O, file synchronization |
| **Page Abstraction** | **Implemented** | 16-byte header with `PageLSN`, structural validation, 4080-byte payload |
| **Buffer Pool Manager** | **Implemented** | LRU eviction, frame table, pin/unpin tracking, dirty flush, WAL ordering |
| **B+ Tree Index (Search & Insert)** | **Implemented** | Dynamic insertion, node splitting, root elevation, point search |
| **B+ Tree Index (Delete & Scan)** | **Implemented** | Deletion, leaf/internal rebalance, root collapse, range scan |
| **Record Store** | **Implemented** | Slotted-page tuple format, variable-length records, local compaction |
| **Multi-Page Overflow Records** | **Implemented** | Singly-linked overflow chains (`PageType::Overflow`), arbitrary value sizes, stable `RecordId`s, freelist recycling |
| **TreeDB Engine API** | **Implemented** | High-level `TreeDb` interface for `put`, `get`, `delete`, `scan`, `sync` |
| **Write-Ahead Logging** | **Implemented** | Monotonic `Lsn`, CRC32 log records, `.wal` file, durable syncing, WAL ordering on flush/evict |
| **Crash Recovery (ARIES-Inspired)** | **Implemented** | 3-Phase recovery (Analysis, Redo, Undo with CLRs), restart-safe, deterministic fault injection, idempotence |
| **Free-Page Recycling & Freelist** | **Implemented** | Persistent singly-linked freelist, $O(1)$ LIFO reuse, B+ Tree merge/collapse reclamation, integrity validation |
| **Checkpointing & WAL Lifecycle** | **Implemented** | Synchronous checkpoints, durable Page 0 metadata, DPT Redo optimization, safe retention boundary, bounded WAL growth |
| **WAL Lifecycle & Checkpoint Hardening** | **Implemented** | Fundamental retention invariant, active txn Undo preservation, monotonic LSN continuity, non-zero start LSN, crash-safe atomic replacement, .wal.bak fallback, 4-path equivalence |
| **Transaction Manager & Atomic Multi-Op Txns** | **Implemented** | Explicit `Transaction<'db>` RAII API, multi-operation atomic commit/abort, CLR runtime rollback, drop safety, read-your-writes, crash recovery |
| **Persistent Catalog & Schema Metadata** | **Implemented** | Stable monotonic `TableId`s, `TableMetadata`, `PageType::Catalog`, paged chaining, transactional create/drop, freelist reclamation, catalog validation |
| **Typed Tables & Tuple Storage** | **Implemented** | Strong typing (`Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`), null bitmaps, canonical index key encoding, `Table<'a>` CRUD API, dynamic root synchronization |
| **Query Execution & Expression Engine** | **Implemented** | Scalar expression AST, SQL 3-valued logic (3VL), composable operators (`Scan`, `Filter`, `Project`, `Sort`, `Limit`, `Offset`), PK point & range scan optimizations, programmatic `Query` builder |
| **SQL Lexer, Parser & AST** | **Implemented** | Zero-dependency lexical scanner with byte-offset span tracking, Pratt precedence climbing parser, AST representation for scoped `SELECT` dialect |
| **SQL Binding & Execution Engine** | **Implemented** | Semantic analysis, table/column name resolution, wildcard expansion, type checking, transaction-aware query execution |
| **Persistent Secondary Indexes** | **Implemented** | Monotonic `IndexId`s, `IndexMetadata`, canonical order-preserving index key encoding, unique/non-unique composite keys, auto-maintenance on table mutations, query index access paths, catalog validation |
| **Aggregation, GROUP BY & HAVING** | **Implemented** | `COUNT(*)`, `COUNT(expr)`, `SUM`, `AVG`, `MIN`, `MAX`, single/multi-key `GROUP BY`, `HAVING` filter, deterministic `CanonicalValue` grouping, incremental accumulators, semantic validation |
| **Relational JOINs** | **Implemented** | `INNER JOIN`, `LEFT JOIN` / `LEFT OUTER JOIN`, multi-table chained joins, table aliases, qualified/unqualified column ambiguity resolution, 3VL `ON` predicates, NULL padding, full query composition with WHERE, GROUP BY, HAVING, ORDER BY, LIMIT/OFFSET |
| **Rule-Based Optimizer & Access-Path Planner** | **Implemented** | Decoupled logical/physical planning, deterministic access path selection (PK point/range, secondary index point/range, full scan), AND predicate decomposition, safe join pushdown, LEFT JOIN outer safety, residual filter preservation, plan inspection (`explain_sql`) |
| **SQL DDL & DML Engine** | **Implemented** | `CREATE TABLE`, `DROP TABLE`, `INSERT INTO ... VALUES`, `UPDATE ... SET`, `DELETE FROM`, optimizer reuse for mutation access paths, transaction rollback, secondary index auto-maintenance, structured `StatementResult` |
| **SQL Subqueries & Correlation** | **Implemented** | Scalar subqueries (0 rows -> `NULL`, 1 row -> value, >1 rows -> error), `IN` / `NOT IN` with SQL 3VL, `EXISTS` / `NOT EXISTS`, nested binding scopes, outer column references, subquery optimizer integration, uncorrelated pre-evaluation & materialization |
| **Common Table Expressions (CTEs)** | **Implemented** | Non-recursive `WITH cte AS (...) SELECT ...`, multiple CTEs, dependency chains, explicit column renaming `cte(c1, c2)`, table shadowing, cycle detection, optimizer integration, statement-local in-memory materialization, transaction visibility |
| **SQL Window Functions** | **Implemented** | Dedicated `WindowOperator`, ranking (`ROW_NUMBER`, `RANK`, `DENSE_RANK`), navigation (`LAG`, `LEAD`), value functions (`FIRST_VALUE`, `LAST_VALUE`), `PARTITION BY`, `ORDER BY`, `ROWS` frames, default framing, deterministic tie-breaking, CTE & JOIN composition |
| **Recursive CTEs (WITH RECURSIVE)** | **Implemented** | Iterative `WITH RECURSIVE cte AS (anchor UNION ALL recursive) SELECT ...`, delta working-set loop, iteration & row limit safeguards, schema validation, mutual recursion rejection, composition with joins, filters, subqueries, and window functions |
| **SQL Set Operations** | **Implemented** | `UNION`, `UNION ALL`, `INTERSECT`, `INTERSECT ALL`, `EXCEPT`, `EXCEPT ALL`, precedence (`INTERSECT` > `UNION`/`EXCEPT`), parenthesized query expressions, `NULL = NULL` row identity equality, type coercion & output schema derivation from left operand, multiset arithmetic, subquery/CTE/JOIN/window/order-by integration |
| **SQL Secondary-Index DDL** | **Implemented** | `CREATE INDEX`, `CREATE UNIQUE INDEX`, `DROP INDEX`, single-column index specification, canonical `IndexKey` types, strict single-`NULL` uniqueness, persistent B+ Tree integration, existing row indexing, optimizer access-path visibility, transactional rollback & ARIES recovery |
| **SQL EXPLAIN Query Introspection** | **Implemented** | `EXPLAIN <query>`, query plan introspection, deterministic physical plan formatting, scans, access paths, joins, filters, projections, sorts, window functions, CTEs, recursive CTEs, set operations, subqueries, no execution side-effects, transaction visibility |
| **Query Statistics & Cost-Based Optimization** | **Implemented** | Table/column/index statistics collection (`collect_statistics`, `ANALYZE <table>;`), `StatsRegistry`, deterministic `CostModel`, `CardinalityEstimator` (equality, ranges, nulls, 3VL logic, equi-joins), cost-based access-path selection (PK lookup/range, secondary index lookup/range, full scan), bounded inner-join ordering (2–5 relations), EXPLAIN cost/row estimation, missing-stats fallback |
| **Query Execution Metrics & Runtime Plan Diagnostics** | **Implemented** | Fine-grained physical operator metrics (`OperatorMetrics`, `QueryExecutionMetrics`), runtime row counters (examined, passed, rejected, joined, aggregated, sorted), `EXPLAIN ANALYZE <query>`, estimated vs. actual cardinality comparison, estimate error ratio, diagnostic-only wall-clock timing, explicit `execute_sql_with_metrics` API, transaction visibility & read-only safety, unsupported DML error rejection |
| **Prepared Statements & Parameter Binding** | **Implemented** | Positional parameter syntax `?` (1-indexed), explicit `SqlExpr::Parameter` and `Expr::Parameter` AST/bound representation, parameter type inference from SQL context (`ParameterMetadata`), `PreparedStatement` lifecycle, validation of parameter counts and types, SQL 3VL NULL semantics, value-driven query optimization (PK point lookup, PK range scan, secondary index scan), DML prepared statements (`INSERT`, `UPDATE`, `DELETE`), subqueries, CTEs, recursive CTEs, JOINs, window functions, set operations, EXPLAIN & EXPLAIN ANALYZE parameter integration, SQL injection immunity |
| **Physical Plan Cache & Prepared-Statement Reuse** | **Implemented** | Invalidation-aware, bounded, deterministic, parameter-sensitive physical plan cache (`PlanCache`, `PlanCacheKey`, `PlanValidity`, `PlanCacheStats`), monotonic generation counters (`catalog_generation`, `index_generation`, `statistics_generation`), multi-level invalidation on DDL and statistics (`CREATE`/`DROP TABLE`, `CREATE`/`DROP INDEX`, `ANALYZE`), transaction DDL isolation, LRU eviction, runtime-only zero-persistence architecture, `plan_cache_hit` execution metrics |
| **External Sort & Spill-to-Disk Execution** | **Implemented** | Bounded in-memory sort with spill-to-disk execution (`ExternalSorter`, `SortConfig`, `TempRunFile`, `RunReader`, `SortExecutionMetrics`), deterministic binary run format, exact SQL ORDER BY semantics preservation (ASC/DESC, NULLS FIRST/LAST, multi-key, NaN total_cmp), stable sequence tie-breaking, multi-pass k-way merge, RAII temporary file cleanup, WAL and database page exclusion, EXPLAIN ANALYZE telemetry |
| **External Aggregation & Spill-to-Disk GROUP BY** | **Implemented** | Bounded in-memory hash aggregation with partitioned spill-to-disk execution (`ExternalAggregator`, `AggregateConfig`, `TempAggregateFile`, `AggregateExecutionMetrics`), deterministic binary partition format, exact SQL grouping and aggregation semantics preservation (`COUNT(*)`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, NULL keys, Float64 NaN), algebraic state merging, multi-pass recursive sub-partitioning, RAII temporary file cleanup, WAL and database page exclusion, EXPLAIN ANALYZE telemetry |
| **External Hash Join & Spill-to-Disk JOIN** | **Implemented** | Bounded in-memory hash join with partitioned spill-to-disk execution (`ExternalHashJoiner`, `JoinConfig`, `TempJoinFile`, `JoinExecutionMetrics`), deterministic binary partition format, exact SQL join semantics preservation (`INNER JOIN`, `LEFT JOIN`, composite equi-join keys, residual predicates, SQL 3VL `NULL != NULL`), salted multi-pass recursive sub-partitioning, RAII temporary file cleanup, WAL and database page exclusion, EXPLAIN ANALYZE telemetry |
| **RIGHT & FULL OUTER JOINs & Generalized Planning** | **Implemented** | Generalized outer join execution (`RIGHT JOIN`, `RIGHT OUTER JOIN`, `FULL JOIN`, `FULL OUTER JOIN`), dual-side match bitset tracking, residual predicate gating, dynamic schema nullability derivation, spilled outer partition preservation, WHERE pushdown safety, plan caching & telemetry |
| **Cost-Based Join Reordering & Join Enumeration** | **Implemented** | Deterministic cost-based join reordering inside safe `INNER JOIN` regions (`JoinGraph`, `JoinGraphRelation`, `JoinGraphPredicate`), subset Dynamic Programming ($O(n \cdot 2^n)$) for $N \le 8$, deterministic $O(n^2)$ greedy candidate fallback ($N > 8$), strict outer-join optimization barriers (`LEFT`, `RIGHT`, `FULL`), 5-tier deterministic tie-breaking, earliest legal predicate placement, plan caching & telemetry |
| **Query Rewriter, Subquery Decorrelation & Explain Analyze** | **Implemented** | Safe derived-table flattening, predicate pushdown, subquery decorrelation (`EXISTS`, `NOT EXISTS`, `IN` to SEMI/ANTI joins), alias qualification mapping, 3VL NULL preservation, runtime plan execution telemetry, physical column pruning deferred |
| **Cost-Based SEMI/ANTI Join Planning** | **Implemented** | Cost-based SEMI and ANTI join optimization and physical execution planning (`JoinStrategy::NestedLoop`, `Hash`, `IndexProbe`), index existence probes with tuple-fetch skipping, calibrated CPU/probe cost modeling, semi/anti cardinality estimation, plan cache invalidation tracking, SQL duplicate elimination and 3VL NULL isolation, comprehensive EXPLAIN ANALYZE runtime metrics |
| **Composite Secondary Indexes** | **Implemented** | Multi-column persistent secondary indexes (`CREATE [UNIQUE] INDEX ... ON table (col1, col2)`), canonical lexicographical composite key encoding, leading-prefix scans, range scans, strict multi-column single-NULL uniqueness, atomic backfill, cost-based optimizer integration |
| **Covering Indexes & Index-Only Execution** | **Implemented** | Covering secondary indexes with `INCLUDE (col3, col4)`, versioned `CoveredPayload` binary format, index-only physical operator (`IndexOnlyScan`), zero base-record fetches (`base_records_avoided`), residual predicate evaluation on covered values, ORDER BY, GROUP BY, window, and JOIN integration, comprehensive EXPLAIN ANALYZE metrics |
| **Cost-Based Index Skip Scan** | **Implemented** | Cost-based skip scan over composite secondary indexes (`IndexSkipScan`, `IndexOnlySkipScan`), streaming distinct prefix enumeration (`distinct_prefix_values`), suffix probe matching, statistics-driven cost modeling, covering/index-only integration, safe ORDER BY and early LIMIT/OFFSET termination |
| **MVCC & Version Chains** | **Implemented** | Slotted tuple delta chains, snapshot visibility filtering (`SnapshotId`, `CommitSeq`), non-blocking readers, transaction isolation, write-set tracking |
| **Vacuum & Space Reclamation** | **Implemented** | Historical version pruning behind reclamation horizon, empty record page reclamation, overflow page deallocation, freelist recycling |
| **Cross-Layer Consistency Verification** | **Implemented** | Non-destructive structural and logical audit (`verify_consistency`, `verify_file`, `check_file`), freelist cycle detection, index payload matching |
| **Recovery & Checkpoint Certification** | **Implemented** | ARIES-inspired 3-phase recovery (Analysis, Redo, Undo with CLRs), fuzzy checkpoints, monotonic `CommitSeq` preservation, deterministic failure injection |
| **Consistent Backup & Safe Restore** | **Implemented** | Self-describing CRC32 manifests (`treedb.manifest`), companion WAL inclusion, two-phase publication protocol (`RestoreJournal`), deterministic crash recovery |

---

## Release Status

* **Current Version:** `v0.1.0` (General Availability Release Packaging)
* **Release Candidate:** `v0.1.0-rc1` (frozen at commit `22dfc2ef6647f9b4f778dd5bde91d142e0ecc867`)
* **Test Suite:** **2,105 passing tests** across 91 test-bearing binaries (48 unit + 2,057 integration across 90 suites; 0 failures, 0 warnings, 0 ignored)
* **Language & Safety:** 100% Safe Rust (`#![forbid(unsafe_code)]`; zero `unsafe` blocks)
* **Durability:** Write-Ahead Logging (WAL) with PageLSN ordering and ARIES crash recovery certified
* **Distribution Packaging:** Clean, verified Cargo package (`cargo package`) containing zero extraneous artifacts

---

## Getting Started

### Prerequisites
* Rust 1.80+ (Stable toolchain)

### Building
```bash
cargo build --release
```

### Running Tests
```bash
cargo test --all
```

### Running the CLI
```bash
# Initialize a new database
cargo run --bin treedb -- init test.treedb

# Inspect database file metadata
cargo run --bin treedb -- info test.treedb

# Execute SQL DDL and DML statements
cargo run --bin treedb -- query test.treedb "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);"
cargo run --bin treedb -- query test.treedb "INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25);"
cargo run --bin treedb -- query test.treedb "SELECT name, age FROM users WHERE age >= 21 ORDER BY age DESC;"
```

### Minimal Rust Public API Example

```rust
use treedb::{TreeDb, Result};

fn main() -> Result<()> {
    // 1. Create a new database with a 64-page buffer pool (or use TreeDb::open for existing)
    let mut db = TreeDb::create("mydb.treedb", 64)?;

    // 2. Execute DDL and DML statements
    db.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT, balance INT);")?;
    db.execute_statement("INSERT INTO accounts VALUES (1, 'Alice', 1000), (2, 'Bob', 500);")?;

    // 3. Query records
    let res = db.execute_sql("SELECT owner, balance FROM accounts WHERE balance > 600;")?;
    for row in res.rows() {
        println!("Owner: {}, Balance: {}", row.values[0], row.values[1]);
    }

    // 4. Force synchronization to disk
    db.sync()?;

    // 5. Run non-destructive consistency audit
    let report = db.verify_consistency()?;
    assert!(report.is_healthy());

    Ok(())
}
```

---

## Persistence & Crash Recovery Overview

TreeDB provides durable, recoverable single-node persistence:

1. **Write-Ahead Logging (WAL)**: Mutations log redo and undo records before dirty in-memory pages are evicted or flushed. Flushed `Commit` records guarantee transaction durability.
2. **ARIES-Inspired Recovery**: Upon restart or crash recovery, TreeDB runs:
   * **Analysis**: Scans forward from the checkpoint LSN to rebuild active transaction and dirty page tables.
   * **Redo**: Repeats history forward, ensuring all committed changes are replayed in storage.
   * **Undo**: Rolls back active uncommitted transactions in reverse, logging Compensation Log Records (CLRs) for restart safety.
3. **Fuzzy Checkpoints**: Flushes dirty pages, writes checkpoint records, and truncates obsolete WAL records while preserving active transaction undo segments.
4. **Crash-Resilient Restore (`RestoreJournal`)**: Restoring multi-file database/WAL pairs coordinates through an on-disk journal (`.restore_journal`), guaranteeing that interrupted operations deterministically roll back to the original database or roll forward to the new database.

---

## Operational Constraints & Limitations

TreeDB is designed for single-node embedded storage exploration:

* **Single-Writer Exclusivity**: TreeDB assumes single-process, single-writer access. Concurrent uncoordinated multi-process writes are not supported.
* **Offline Backups Only**: Backups must be performed when the database is quiescent (no active write transactions). Online hot backup during concurrent writes is not supported.
* **No Heuristic Auto-Repair**: Damaged headers or corrupted pages fail loudly rather than attempting heuristic repair, preventing silent corruption.

---

## Architectural Principles

1. **Correctness before performance:** Invariants are strictly verified through tests and structural validation before applying optimizations.
2. **Explicit invariants:** Types and checks enforce internal limits and prevent corruption.
3. **Strict layer separation:** Higher-level abstractions (B+ Tree, Buffer Pool) interact exclusively through clean lower-level interfaces.
4. **Deterministic persistent format:** On-disk binary representation is explicitly specified with little-endian integer encoding.
5. **Safe Rust ownership:** Avoiding unsafe code wherever possible by leveraging Rust's ownership model for lifecycle management.
6. **Write-Ahead Durability:** Dirty pages are never flushed before their corresponding WAL records.

---

## Documentation

* [Release Notes v0.1.0](docs/RELEASE_NOTES_v0.1.0.md)
* [Release Candidate Validation](docs/RELEASE_CANDIDATE_VALIDATION.md)
* [Release Candidate Audit](docs/RELEASE_CANDIDATE_AUDIT.md)
* [Persistence Certification](docs/PERSISTENCE_CERTIFICATION.md)
* [Backup & Restore Guide](docs/BACKUP_RESTORE.md)
* [Cross-Layer Consistency Verification](docs/STORAGE_CONSISTENCY.md)
* [Recovery & Checkpoint Certification](docs/RECOVERY_CERTIFICATION.md)
* [Transaction Atomicity & Crash Testing](docs/TRANSACTION_ATOMICITY.md)
* [Architecture Overview](docs/ARCHITECTURE.md)
* [B+ Tree Index Engine](docs/BPLUS_TREE.md)
* [Buffer Pool Manager](docs/BUFFER_POOL.md)
* [On-Disk Storage Format](docs/STORAGE_FORMAT.md)
* [Design Decisions](docs/DESIGN_DECISIONS.md)
* [Project Roadmap](docs/ROADMAP.md)

---

## License

Dual-licensed under either of:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
