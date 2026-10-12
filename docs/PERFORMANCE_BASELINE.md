# TreeDB Performance Baseline Report (Phase 81)

## Executive Summary

This document establishes the official performance baselines for **TreeDB v0.1.0** following its General Availability release packaging. TreeDB is an ACID-compliant, persistent relational storage engine built from first principles in Rust.

The objective of Phase 81 is strictly empirical: establish reproducible, deterministic benchmarks across all core subsystems (B+ Tree indexing, Record Store, Buffer Pool & Page Management, WAL & Transaction processing, SQL & Query Execution, and MVCC & Vacuuming) before embarking on optimization cycles.

### Summary of Key Baseline Findings

| Subsystem | Workload / Operation | Dataset Size | Throughput | Avg Latency | p50 Latency | p99 Latency |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **B+ Tree** | In-Memory Point Lookup Hit | 10,000 keys | 1,260,104 ops/s | 793.6 ns | 800.0 ns | 1.80 µs |
| **B+ Tree** | In-Memory Point Lookup Hit | 50,000 keys | 650,823 ops/s | 1.54 µs | 1.40 µs | 4.00 µs |
| **B+ Tree** | Insert into Pre-populated Tree | 50,000 keys | 430,867 ops/s | 2.32 µs | 1.90 µs | 9.20 µs |
| **Record Store** | Sequential 64B Record Read | 5,000 recs | 1,678,359 ops/s | 595.8 ns | 600.0 ns | 1.60 µs |
| **Record Store** | Randomized 64B Record Read | 5,000 recs | 2,131,105 ops/s | 469.2 ns | 400.0 ns | 1.20 µs |
| **Record Store** | Sequential 8KB Overflow Read | 500 recs | 49,120 ops/s | 20.36 µs | 16.80 µs | 61.40 µs |
| **Record Store** | Overflow 8KB Deletion & Freelist | 500 recs | 541 ops/s | 1.85 ms | 1.57 ms | 4.58 ms |
| **Buffer Pool** | Warm Cache Point Access | 2,000 pages | 19,219,681 ops/s | 52.0 ns | < 100 ns | 100.0 ns |
| **Buffer Pool** | Cold Cache Point Access (Disk read) | 2,000 pages | 33,703 ops/s | 29.67 µs | 20.30 µs | 299.10 µs |
| **Buffer Pool** | Constrained Cache (3% hit rate) | 2,000 pages | 317,674 ops/s | 3.15 µs | 2.90 µs | 9.50 µs |
| **Buffer Pool** | Dirty Page Flush (500 pages) | 500 pages | 742 flushes/s | 1.35 ms | 1.35 ms | 1.35 ms |
| **Buffer Pool** | Sequential Page Allocation | 1,000 pages | 102,222 ops/s | 9.78 µs | 7.00 µs | 43.10 µs |
| **WAL & Txn** | Read-Only Transaction Lifecycle | 1 row | 3,563 txns/s | 280.7 µs | 240.5 µs | 677.3 µs |
| **WAL & Txn** | Single-Row Write Transaction | 200 rows | 2,266 txns/s | 441.3 µs | 397.8 µs | 1.03 ms |
| **WAL & Txn** | Batch Write (50 rows / commit) | 50 rows | 138.7 txns/s | 7.21 ms | 7.48 ms | 8.60 ms |
| **WAL & Txn** | Transaction Abort & Rollback | 200 rows | 147.9 aborts/s | 6.76 ms | 6.81 ms | 12.57 ms |
| **WAL & Txn** | Storage Durability Sync (`fsync`) | 50 calls | 3,412 syncs/s | 293.1 µs | 263.1 µs | 752.5 µs |
| **WAL & Txn** | ARIES Crash Recovery (1,000 logs) | 1,000 logs | 106.7 recoveries/s| 9.37 ms | 9.37 ms | 9.37 ms |
| **SQL & Query** | PK Point Lookup (`SELECT ... WHERE id=?`)| 2,000 rows | 2,395 queries/s | 417.6 µs | 379.5 µs | 901.0 µs |
| **SQL & Query** | Secondary Index Scan (5% select) | 2,000 rows | 542 queries/s | 1.85 ms | 1.80 ms | 2.69 ms |
| **SQL & Query** | PK Range Scan (100 rows) | 2,000 rows | 485 queries/s | 2.06 ms | 2.02 ms | 2.71 ms |
| **SQL & Query** | Full Table Scan (2,000 rows) | 2,000 rows | 654 scans/s | 1.53 ms | 1.49 ms | 1.93 ms |
| **SQL & Query** | Global Aggregation (`COUNT, SUM, AVG`) | 2,000 rows | 704 queries/s | 1.42 ms | 1.37 ms | 2.38 ms |
| **SQL & Query** | In-Memory Sort (`ORDER BY`) | 2,000 rows | 46.5 queries/s | 21.53 ms | 21.74 ms | 24.30 ms |
| **SQL & Query** | In-Memory Hash Join (`2,000 ⋈ 4,000`) | 4,000 rows | 28.4 queries/s | 35.23 ms | 33.68 ms | 55.29 ms |
| **SQL & Query** | Prepared Statement Execution | 2,000 rows | 2,414 queries/s | 414.2 µs | 391.8 µs | 824.5 µs |
| **SQL & Query** | Plan Cache Hit Execution | 2,000 rows | 38,728 queries/s | 25.8 µs | 25.40 µs | 48.40 µs |
| **MVCC & Vac** | Point Read Depth 0 (Clean) | 500 rows | 3,349 queries/s | 298.6 µs | 280.2 µs | 571.8 µs |
| **MVCC & Vac** | Version Chain Gen (5 updates/row) | 2,500 updates | 2,022 updates/s | 494.6 µs | 494.6 µs | 494.6 µs |
| **MVCC & Vac** | Point Read Depth 5 (Chain Traversal)| 500 rows | 3,236 queries/s | 309.0 µs | 290.3 µs | 610.9 µs |
| **MVCC & Vac** | Full Scan with Active Chains | 500 rows | 1,479 scans/s | 676.1 µs | 643.2 µs | 1.14 ms |
| **MVCC & Vac** | Online Vacuum Reclaim (2,500 ver) | 500 rows | 11.9 vacuums/s | 83.93 ms | 83.93 ms | 83.93 ms |
| **MVCC & Vac** | Point Read Post-Vacuum | 500 rows | 3,195 queries/s | 313.0 µs | 286.4 µs | 839.2 µs |

---

## 1. Benchmarking Principles & Methodology

1. **Isolation & Repeatability**:
   Each benchmark executes in isolated temporary directories (`tempfile::tempdir()`), ensuring zero contaminated state between test runs.
2. **Determinism**:
   Key ordering, access shuffling, and payload content are generated with a deterministic pseudo-random number generator (`XorShift64`) with fixed seeds (`0x4D59_5345_4544_3831`).
3. **Setup Separation**:
   Table schema creation, B+ Tree population, baseline data loading, and index builds are executed prior to sample collection. The timing clock records only steady-state operation latency.
4. **Correctness Verification**:
   Before and after timed intervals, correctness assertions verify row counts, key existence, checksums, and transactional invariants without adding overhead to the inner measurement loop.
5. **Statistical Metrics**:
   Sample measurements capture complete latency distributions, recording Minimum, Average, Maximum, and Percentiles (p50, p95, p99), alongside operation throughput (ops/sec).

---

## 2. Environment and Hardware Information

* **Operating System**: Windows 11 Pro (x86_64-pc-windows-msvc)
* **Processor (CPU)**: 13th Gen Intel(R) Core(TM) i7-1355U (10 Physical Cores: 2 Performance, 8 Efficient; 12 Logical Processors; Base 1.7 GHz, Turbo up to 5.0 GHz)
* **Toolchain**: `rustc 1.98.1 (48a229cea 2026-09-01)` / `cargo 1.98.1`
* **Compilation Profile**: `--profile bench` (`opt-level = 3`, LTO enabled, debug assertions disabled)
* **Storage Subsystem**: NVMe Solid State Drive (NTFS filesystem, Windows write caching active)
* **Operating Model**: Single-writer storage engine with concurrent multi-reader MVCC snapshots.

---

## 3. Workload Category Analysis

### 3.1 B+ Tree Subsystem

* **Key Scales**: 1,000 keys (depth 2), 10,000 keys (depth 3), 50,000 keys (depth 3–4).
* **Point Lookups**: Hit lookups scale logarithmically. At 1,000 keys, average latency is 887 ns (1.13M ops/s); at 10,000 keys, it is 793.6 ns (1.26M ops/s); at 50,000 keys, it increases to 1.54 µs (650k ops/s) due to increased leaf and interior page binary searches.
* **Inserts**: Inserting into a pre-populated tree takes 1.48 µs (1k), 1.57 µs (10k), and 2.32 µs (50k). Leaf splits and occasional root splits account for tail latencies observed at p99 (up to 9.2 µs for 50k keys).
* **Deletions**: Deletion requires leaf underflow handling and tree rebalancing/borrowing, yielding an average latency of 1.55 µs – 2.26 µs.

### 3.2 Record Store Subsystem

* **Inline Records (64B & 512B)**:
  * 64B inline reads achieve 1.68M – 2.13M ops/s (469 ns – 596 ns avg latency).
  * 512B inline reads achieve 1.67M – 3.02M ops/s (330 ns – 597 ns avg latency).
  * Record slot reclamation and in-page deletion take ~451 ns (2.2M ops/s).
* **Overflow Records (8KB & 16KB)**:
  * An 8KB record spans 3 pages (4,096-byte page size). Sequential reading takes 20.36 µs (49.1k ops/s); randomized reading takes 15.96 µs (62.7k ops/s).
  * A 16KB record spans 5 pages. Sequential reading takes 28.04 µs (35.7k ops/s).
  * **Critical Observation**: Deleting overflow records takes 1.85 ms (8KB) to 2.47 ms (16KB) due to sequential traversal and updating of the persistent freelist for each unlinked overflow page.

### 3.3 Buffer Pool & Page Manager Subsystem

* **Warm Cache vs. Cold Cache**:
  * **Warm Cache**: In-memory frame access via frame table lookup and reader pin takes **52.0 ns** (19.22 Million ops/sec).
  * **Cold Cache**: Un-cached page access requiring synchronous disk read from NTFS storage takes **29.67 µs** (33.7k ops/sec).
  * **Gap**: Cold reads are ~570× slower than in-memory buffer pool hits.
* **Cache Eviction Pressure**:
  * Under severe cache constraints (64 frames for 2,000 active pages, resulting in a 3.0% hit rate and 4,784 evictions), average access latency rises to **3.15 µs** (317.7k ops/sec).
* **Dirty Page Flushing**:
  * Flushing 500 dirty pages through `PageManager::write_page` takes **1.35 ms** (~2.7 µs per dirty page).
* **Freelist Deallocation**:
  * Returning 1,000 freed pages to the persistent freelist head takes **885.9 µs** (886 ns per page).

### 3.4 WAL and Transaction Processing

* **Read-Only Transactions**:
  * Starting an MVCC transaction, reading a tuple, and committing takes **280.7 µs** (3,563 txns/s). Read-only commits do not emit WAL log records.
* **Single-Row Write Transactions**:
  * Single-statement transaction with ARIES log record append and state commit takes **441.3 µs** (2,266 txns/s).
* **Batch Write Transactions**:
  * Batching 50 records into a single transaction takes **7.21 ms** (138.7 txns/s, or 6,935 rows/sec).
* **Abort / Rollback Cost**:
  * Aborting an active transaction with Compensation Log Records (CLRs) and write-set restoration takes **6.76 ms** (147.9 aborts/s).
* **Durability Barrier (`FlushFileBuffers` / `fsync`)**:
  * An explicit durability flush takes **293.1 µs** (3,412 fsyncs/sec) on local NVMe storage.
* **ARIES Recovery**:
  * Full 3-phase crash recovery (Analysis, Redo, Undo) over 1,000 log records takes **9.37 ms** (106.7 full recoveries/sec).

### 3.5 SQL and Query Execution Engine

* **Point Lookup via Primary Index**:
  * End-to-end SQL query execution (`SELECT * FROM accounts WHERE id = ?`) takes **417.6 µs** (2,395 queries/sec).
* **Range Scan & Secondary Index**:
  * Primary index range scan of 100 rows takes **2.06 ms** (485 queries/sec).
  * Secondary index lookup (5% selectivity, 100 rows) takes **1.85 ms** (542 queries/sec).
* **Scan & Aggregation**:
  * Full table scan of 2,000 rows takes **1.53 ms** (654 scans/sec).
  * Global aggregation (`COUNT(*)`, `SUM`, `AVG`) takes **1.42 ms** (704 queries/sec).
* **Sort & Hash Join**:
  * In-memory `ORDER BY` sorting over 2,000 rows takes **21.53 ms** (46.5 queries/sec).
  * Equi-hash join between 2,000 accounts and 4,000 log records takes **35.23 ms** (28.4 joins/sec).
* **Prepared Statements vs. Plan Cache Hits**:
  * Standard prepared statement execution with parameter binding takes **414.2 µs** (2,414 queries/sec).
  * Plan Cache hit execution (reusing the optimized physical plan without re-running the cost-based optimizer) takes **25.8 µs** (**38,728 queries/sec**).
  * **Observation**: Plan caching provides a **16.0× end-to-end query throughput improvement**.

### 3.6 MVCC and Vacuuming

* **Version Chain Depth Impact**:
  * Reading at version depth 0: **298.6 µs** (3,349 queries/sec).
  * Reading latest version with 5 historical versions present: **309.0 µs** (3,236 queries/sec).
  * **Observation**: In-page MVCC chain indexing keeps point read overhead minimal (+3.5% latency) for moderate chain depths.
* **Scan Degradation with Version Bloat**:
  * Full table scan over 500 rows with 5 historical versions per row takes **676.1 µs**, compared to clean scans, as obsolete row versions are evaluated and filtered out by snapshot visibility.
* **Online Vacuuming**:
  * Vacuuming a bloat of 2,500 historical versions across 62 examined pages reclaimed 2,500 versions and freed 51 pages in **83.93 ms**.
* **Post-Vacuum Recovery**:
  * Point read latency after vacuuming returned to **313.0 µs** (3,195 queries/sec), confirming pruning of dead versions.

---

## 4. Known Measurement Limitations

1. **OS Page Cache Interference**:
   Tests run on local NTFS storage. While cold-cache benchmarks initialize distinct temporary databases, the Windows OS page cache may cache filesystem metadata and recently written blocks.
2. **Single-Threaded Execution**:
   TreeDB's documented concurrency model is single-writer with multi-version read snapshots. Benchmarks measure single-thread sequential latency and do not test multi-threaded client multiplexing.
3. **Synthetic Cardinality**:
   SQL benchmarks use uniform and modulo-based data distributions. Highly skewed distributions (Zipfian) may show different plan cache invalidation and join bucket clustering characteristics.
4. **Desktop Power States**:
   Measurements were conducted on an Intel Core i7-1355U mobile processor subject to dynamic frequency scaling and thermal throttling.

---

## 5. Evidence-Based Optimization Opportunities

Based strictly on measured data, future optimization phases should focus on:

1. **Overflow Record Deallocation (Record Store / Freelist)**:
   * *Evidence*: Deleting an overflow record requires 1.85 ms – 2.47 ms (only 400–540 ops/s), compared to 451 ns for inline records.
   * *Opportunity*: Batching freelist link insertions and deferring individual header syncs during multi-page deallocation.
2. **In-Memory Sort Operator (`ORDER BY`)**:
   * *Evidence*: Sorting 2,000 rows consumes 21.5 ms (46.5 ops/s), which dominates query execution time.
   * *Opportunity*: Vectorized sort comparisons, chunked radix sort for numeric keys, or index-assisted order pushdown.
3. **Hash Join Probe Acceleration**:
   * *Evidence*: Hash joining 2k × 4k rows takes 35.2 ms.
   * *Opportunity*: Pre-sized hash tables with compact hash keys to reduce allocation and cache misses during the build and probe phases.
4. **Wider Plan Cache Utilization**:
   * *Evidence*: Plan cache hits reduce execution latency from 414 µs to 25.8 µs (a 16× acceleration).
   * *Opportunity*: Extending plan caching to auto-parameterized ad-hoc queries, not solely prepared statements.

---

## 6. Reproduction Commands

To reproduce all measurements under the `bench` profile:

```bash
# Run the complete TreeDB benchmark suite and update CSV results
cargo bench --bench benchmark_suite

# Run specific subsystem benchmarks
cargo bench --bench benchmark_suite -- --category bplus_tree
cargo bench --bench benchmark_suite -- --category record_store
cargo bench --bench benchmark_suite -- --category buffer_pool
cargo bench --bench benchmark_suite -- --category wal_txn
cargo bench --bench benchmark_suite -- --category sql_query
cargo bench --bench benchmark_suite -- --category mvcc_vacuum

# Export results to custom CSV file
cargo bench --bench benchmark_suite -- --csv my_baseline.csv
```
