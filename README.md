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
| **Prepared Statements & Parameter Binding** | **Implemented** | Positional parameter syntax `?` (1-indexed), explicit `SqlExpr::Parameter` and `Expr::Parameter` AST/bound representation, parameter type inference from SQL context (`ParameterMetadata`), `PreparedStatement` lifecycle, validation of parameter counts and types, SQL 3VL NULL semantics, value-driven query optimization (PK point lookup, PK range scan, secondary index scan), DML prepared statements (`INSERT`, `UPDATE`, `DELETE`), subqueries, CTEs, recursive CTEs, JOINs, window functions, set operations, EXPLAIN & EXPLAIN ANALYZE parameter integration, SQL injection immunity, explicit non-caching of physical plans |


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
cargo test --all-targets
```

### Running the CLI
```bash
# Initialize a new database
cargo run --bin treedb -- init test.treedb

# Inspect database file metadata
cargo run --bin treedb -- info test.treedb

# Execute an SQL SELECT query
cargo run --bin treedb -- query test.treedb "SELECT name, age FROM users WHERE age >= 21 ORDER BY age DESC LIMIT 10;"
```


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

* [Architecture Overview](docs/ARCHITECTURE.md)
* [B+ Tree Index Engine](docs/BPLUS_TREE.md)
* [Buffer Pool Manager](docs/BUFFER_POOL.md)
* [On-Disk Storage Format](docs/STORAGE_FORMAT.md)
* [Design Decisions](docs/DESIGN_DECISIONS.md)
* [Project Roadmap](docs/ROADMAP.md)

---

## License

Dual-licensed under either of:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE) or http://opensource.org/licenses/MIT)
