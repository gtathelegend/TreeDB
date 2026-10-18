# TreeDB — Systems Architecture & Engineering Portfolio

> **A persistent relational database engine built from first principles in 100% Safe Rust (`#![forbid(unsafe_code)]`).**

---

## 1. Architectural Thesis & Objectives

Modern database storage engines (e.g., SQLite, PostgreSQL, RocksDB, WiredTiger) are predominantly implemented in C or C++, where memory safety, pointer arithmetic, and concurrency hazards require immense vigilance. 

**TreeDB** was engineered to answer a core systems-programming question:
*Can a full-featured, disk-backed persistent relational database engine with ARIES crash recovery, slotted-page storage, B+ tree indexing, MVCC snapshot isolation, and a cost-based query optimizer be constructed entirely from first principles in **100% Safe Rust** without compromising correctness or architectural realism?*

### Core Architectural Decisions
1. **Zero `unsafe` Blocks**: Every subsystem—slotted pages, B+ tree splits/merges, buffer pool frame tables, and binary serializers—operates under `#![forbid(unsafe_code)]`. Memory safety and pointer boundaries are strictly verified at compile-time and through explicit boundary invariants.
2. **Explicit Binary Layouts**: Avoids undefined behavior or compiler struct packing discrepancies by using explicit little-endian serialization (`to_le_bytes()` / `from_le_bytes()`) across all 4,096-byte page headers, slots, and log records.
3. **Decoupled Layer Hierarchy**: Higher layers interact strictly through well-defined trait and module interfaces:
   ```text
   SQL / Planner / Executor
             │
             ▼
      MVCC & Version Store
             │
             ▼
        Record Store ──► Freelist Manager
             │
             ▼
     B+ Tree Index Engine
             │
             ▼
    Buffer Pool Manager (Clock Cache + Frame Table)
             │
             ├──────────────────────────┐ (Write-Ahead Invariant: PageLSN <= FlushedLSN)
             ▼                          ▼
       Page Manager                Log Manager
             │                          │
             ▼                          ▼
     Database File (.treedb)     WAL File (.treedb.wal)
   ```

---

## 2. Deep Subsystem Highlights

### 2.1 Slotted-Page Architecture & Variable-Length Storage
* **Page Layout**: Fixed 4,096-byte pages (`PageId = u32`) begin with a 16-byte header containing page type, `PageLSN`, slot count, and free space offset.
* **Tuple Slits & Compaction**: Variable-length tuple payloads grow downward from page end ($4096 \to 16$), while 4-byte slot pointers grow upward from header end ($16 \to 4096$). In-page fragmentation triggers deterministic local compaction.
* **Overflow Record Chains**: Records exceeding inline capacity ($\approx 4080$ bytes) allocate singly-linked overflow page chains (`PageType::Overflow`). Stable `RecordId`s in the primary page point to the overflow head, allowing transparent streaming of arbitrarily large payloads while preserving B+ tree leaf density.
* **Persistent Freelist**: A persistent singly-linked LIFO freelist on Page 0 recycles unallocated or freed pages in $O(1)$ time, reclaiming disk space without requiring database truncation.

### 2.2 B+ Tree Indexing Engine
* **Primary Key B+ Tree**: Maps 64-bit integer keys (`Key = u64`) to `RecordId` locations. Supports logarithmic point search, dynamic node splitting (50/50 split), root elevation, neighbor node borrowing, and leaf merging.
* **Secondary & Composite Indexes**:
  * Persistent secondary B+ trees map canonical lexicographically ordered `IndexKey` values to primary `RecordId`s.
  * Supports unique constraints (with strict single-NULL uniqueness) and non-unique posting lists.
  * Multi-column composite keys (`(col1, col2)`) enable leading-prefix scans and index skip scans.
* **Covering Indexes**: Secondary indexes with `INCLUDE (col3, col4)` store versioned payloads directly in leaf entries. The optimizer utilizes `IndexOnlyScan` to evaluate queries with zero base-table record fetches (`base_records_avoided`).

### 2.3 Buffer Pool Manager & Frame Table
* **Clock Eviction Policy**: Tracks frame references via a circular clock hand, minimizing synchronization overhead compared to strict LRU while effectively protecting hot working sets.
* **Pin/Unpin Lifecycle**: Concurrent reader and writer references pin frames in memory; dirty victim frames under cache pressure cannot be evicted until unpinned.
* **Write-Ahead Logging Enforcement**: Before any dirty buffer frame reaches physical storage, the buffer pool enforces:
  $$\text{PageLSN} \le \text{FlushedWAL\_LSN}$$
  The associated log manager is forced to flush and sync the WAL before the physical page write is permitted.

### 2.4 Multi-Version Concurrency Control (MVCC)
* **In-Page Version Chains**: Record updates append new tuple versions linked to prior versions via physical `prev_rid` pointers in version headers (`VersionMetadata`).
* **Monotonic Commit Sequences**: Every committed transaction is assigned a strictly increasing `CommitSeq`.
* **Snapshot Isolation**: Reader transactions register immutable point-in-time snapshots (`SnapshotId`). Visibility rules determine whether a version was created before the snapshot horizon and not deleted prior to it, eliminating dirty reads, non-repeatable reads, and phantom read anomalies.
* **Online Background Vacuuming**: Dead tuple versions older than the oldest active snapshot horizon are safely unlinked and reclaimed by `db.vacuum()` without blocking active reader transactions.

### 2.5 ARIES-Style Write-Ahead Logging & Crash Recovery
TreeDB implements the foundational 3-phase ARIES recovery algorithm:
1. **Analysis Phase**: Scans forward from the most recent checkpoint LSN to end-of-log. Reconstructs the active (loser) transaction table and the Dirty Page Table (`recLSN`).
2. **Redo Phase (Repeating History)**: Replays all committed changes and Compensation Log Records forward from $\min(\text{recLSN})$. Page LSN checks (`if rec.lsn <= page.lsn() { continue; }`) guarantee redo idempotency.
3. **Undo Phase**: Rolls back active uncommitted (loser) transactions backward in LSN order. Each undone modification emits a Compensation Log Record (`CLR`) containing an `undo_next_lsn` pointer. CLRs are **never undone**, preventing rollback cascades during crashes during recovery.
4. **Crash-Truncated Tail Resilience**: Detects partial trailing record writes caused by abrupt power failure and truncates the corrupt tail, while strictly rejecting mid-log corruptions with `Error::WalCorruption`.
5. **Repeated Recovery Idempotence**: Automated regression suites certify that running recovery across multiple consecutive crash-reopen cycles produces 0 duplicate page mutations and 0 duplicate abort records.

### 2.6 Cost-Based Query Optimizer & Volcano Execution
* **SQL Frontend**: Zero-dependency recursive descent parser with Pratt precedence climbing supporting DDL, DML, CTEs, recursive CTEs, window functions, and set operations (`UNION`, `INTERSECT`, `EXCEPT`).
* **Memo-Based Optimizer**: Explores alternative logical and physical plans using equivalence groups, physical property enforcement (ordering), and cost evaluation.
* **Cardinality Estimation & Histograms**: Computes equi-depth column histograms and multi-column correlations via `ANALYZE <table>;`.
* **Adaptive Join Reordering**: Subset dynamic programming ($O(n \cdot 2^n)$) for $N \le 8$ tables with greedy candidate fallback for large joins, respecting outer join barriers.
* **Bounded External Execution**: In-memory operators gracefully spill to temporary disk runs when memory budgets are exceeded:
  * External spill sorting with multi-pass $k$-way merge.
  * Partitioned external hash aggregation.
  * Partitioned external hash joins.

---

## 3. Empirical Performance Profile

All benchmarks were collected on an Intel Core i7-1355U (NVMe SSD, NTFS, Rust stable, `--profile bench`) as documented in [PERFORMANCE_BASELINE.md](PERFORMANCE_BASELINE.md).

| Subsystem | Workload / Benchmark | Dataset Scale | Measured Throughput | Avg Latency | p99 Latency |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Buffer Pool** | Warm Cache Page Hit | 2,000 pages | **19,219,681 ops/s** | 52.0 ns | 100 ns |
| **Buffer Pool** | Cold Cache Disk Read | 2,000 pages | 33,703 ops/s | 29.67 µs | 299.1 µs |
| **B+ Tree** | In-Memory Point Lookup | 10,000 keys | **1,260,104 ops/s** | 793.6 ns | 1.80 µs |
| **B+ Tree** | Key Insertion (Splits) | 50,000 keys | 430,867 ops/s | 2.32 µs | 9.20 µs |
| **Record Store** | 64B Record Read | 5,000 records | **2,131,105 ops/s** | 469.2 ns | 1.20 µs |
| **Record Store** | 8KB Overflow Read | 500 records | 49,120 ops/s | 20.36 µs | 61.40 µs |
| **WAL & Txn** | Single-Row Write Txn | 200 rows | 2,266 txns/s | 441.3 µs | 1.03 ms |
| **WAL & Txn** | Durability Sync (`sync_all`) | 50 calls | 3,412 syncs/s | 293.1 µs | 752.5 µs |
| **WAL & Txn** | ARIES Recovery (1,000 logs) | 1,000 logs | 106.7 recoveries/s | 9.37 ms | 9.37 ms |
| **SQL Engine** | PK Point Lookup | 2,000 rows | 2,395 queries/s | 417.6 µs | 901.0 µs |
| **SQL Engine** | Plan Cache Hit Execution | 2,000 rows | **38,728 queries/s** | 25.8 µs | 48.4 µs |
| **SQL Engine** | Hash Join ($2000 \bowtie 4000$) | 4,000 rows | 28.4 queries/s | 35.23 ms | 55.29 ms |
| **MVCC & Vac** | Version Chain Point Read (d=5) | 500 rows | 3,236 queries/s | 309.0 µs | 610.9 µs |
| **MVCC & Vac** | Online Vacuum (2,500 versions)| 500 rows | 11.9 vacuums/s | 83.93 ms | 83.93 ms |

---

## 4. Verification & Testing Rigor

TreeDB enforces an exhaustive testing matrix comprising **2,105 tests across 91 test-bearing binaries**:
1. **Deterministic Failure Injection (`RecoveryFault`)**:
   Simulates catastrophic I/O and process aborts:
   * Crash after WAL append but before commit record flush.
   * Crash after durable commit record flush but before buffer pool dirty page write-back.
   * Crash during Page 0 header persistence.
   * Crash during checkpoint log flush or WAL prefix truncation.
2. **Differential Metamorphic Testing**:
   Continuously validates SQL execution, access paths, and aggregations against an independent in-memory relational reference evaluator (`ReferenceEvaluator`).
3. **Cross-Layer Consistency Auditor (`verify_consistency`)**:
   Non-destructively audits live or corrupted database files, verifying page allocation bitmaps, freelist cycles, B+ tree leaf order, slot offsets, and secondary index pointers.

---

## 5. Educational, Research & Systems Portfolio Takeaways

1. **Idiomatic Rust Systems Programming**: Demonstrates that low-level binary storage formats, byte-level page layouts, and disk caching do not necessitate `unsafe` pointers.
2. **Pedagogical Clarity**: Every subsystem is decoupled with transparent lifecycle state machines, making TreeDB an ideal reference engine for studying database internals.
3. **Production Discipline**: Rigorous adherence to release candidates, provenance tracking, deterministic benchmarking, and defensive durability contracts.

---

## 6. Project Author, Maintainer & Engineering Contributions

**TreeDB** was designed, implemented, and maintained by **Vedaang Sharma** ([info@vedaangsharma.in](mailto:info@vedaangsharma.in)).

### Core Contributions Across the Engine
* **Persistent Storage & Page Management**: Implemented the 4,096-byte slotted-page layout, in-page variable-length tuple packing and downward-growing compaction, singly-linked overflow page chains for records exceeding page bounds, and the persistent LIFO page freelist.
* **Buffer Pool & B+ Tree Indexing**: Built the clock-eviction buffer pool manager with frame table pin/unpin lifecycle guarantees; authored the primary B+ tree engine (50/50 node splitting, borrow/merge rebalancing, root elevation) and secondary/composite index infrastructure with prefix matching and covering index (`INCLUDE`) leaf payloads.
* **Record Store & Relational Engine**: Designed the physical record storage layer, schema catalogs, data type serializers/deserializers, table scanning iterators, and the public programmatic and SQL database APIs.
* **SQL Parsing, Planning & Optimization**: Built the volcano-iterator execution engine, multi-column predicate pushdown, join order optimization (subset dynamic programming and greedy heuristics), memo-based plan exploration, single- and multi-column histogram statistics collection (`ANALYZE`), `EXPLAIN`/`EXPLAIN ANALYZE` introspection, and plan caching.
* **Transactions, MVCC & ARIES Recovery**: Authored the transaction manager supporting MVCC snapshot isolation; designed the Write-Ahead Log (WAL) with strict WAL-before-page flush invariant; engineered the 3-phase ARIES crash-recovery algorithm (Analysis, Redo, Undo via Compensation Log Records) and fuzzy checkpointing with prefix log truncation.
* **Backup, Restore & Crash Resilience**: Engineered the consistent backup system with checksum validation and the crash-resilient `RestoreJournal` two-phase staging protocol.
* **Verification, Examples & Test Isolation**: Authored comprehensive empirical benchmarking suites ([PERFORMANCE_BASELINE.md](PERFORMANCE_BASELINE.md)), 4 standalone runnable examples (`examples/`), `#![forbid(unsafe_code)]` enforcement across the crate, and hermetic `tempfile::TempDir` test isolation across all integration test suites.

### Operational Boundaries & Current Scope
While TreeDB provides production-grade internal invariants, its current scope (`v0.1.0`) explicitly features single-node, single-process write exclusivity, single-file WAL storage, and quiescent offline backup procedures as documented in [DURABILITY_AND_RECOVERY.md](DURABILITY_AND_RECOVERY.md). Multi-node consensus, distributed replication, and multi-process concurrent writers remain future extensions.

