# TreeDB

[![Language](https://img.shields.io/badge/language-Rust%202021-orange.svg)](https://www.rust-lang.org/)
[![Safety](https://img.shields.io/badge/safety-100%25%20Safe%20Rust-brightgreen.svg)](#safe-rust-guarantee)
[![Tests](https://img.shields.io/badge/tests-2%2C105%20passed%20%7C%2091%20suites-blue.svg)](#running-tests)
[![Durability](https://img.shields.io/badge/durability-ARIES%20Crash%20Recovery-purple.svg)](docs/DURABILITY_AND_RECOVERY.md)
[![Release Candidate](https://img.shields.io/badge/status-v0.1.0--rc2%20staged-yellow.svg)](docs/RELEASE_READINESS.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

> **TreeDB is a persistent, relational storage engine built from first principles in 100% Safe Rust (`#![forbid(unsafe_code)]`).**

TreeDB is designed as a deep systems-programming research and educational engine. It implements the foundational components of a modern database management system—slotted-page storage, B+ tree indexing, buffer pool caching, Multi-Version Concurrency Control (MVCC), Write-Ahead Logging (WAL) with ARIES-style crash recovery, and a cost-based SQL query optimizer—without relying on external database libraries or unsafe memory transmutes.

---

## High-Level Architecture

TreeDB enforces strict layer separation. Each layer communicates through explicit boundaries and contracts:

```text
+-------------------------------------------------------------------------+
|                            Application / CLI                            |
+-------------------------------------------------------------------------+
                                     │
                                     ▼
+-------------------------------------------------------------------------+
|                  SQL Frontend & Query Execution Engine                  |
|  • Pratt Precedence Parser     • Cost-Based Memo Optimizer              |
|  • Cardinality & Histograms    • Volcano Iterator Execution Engine      |
|  • Adaptive Join Reordering    • External Spill Sort / Hash / Aggregate |
+-------------------------------------------------------------------------+
                                     │
                                     ▼
+-------------------------------------------------------------------------+
|             Transactions, MVCC & Cross-Layer Verification               |
|  • ACID Multi-Statement Txns   • Monotonic Commit Sequence (CommitSeq)  |
|  • Snapshot Isolation          • In-Page MVCC Version Chains            |
|  • Online Background Vacuum    • Cross-Layer Consistency Auditor        |
+-------------------------------------------------------------------------+
                                     │
                                     ▼
+-------------------------------------------------------------------------+
|                      Relational Catalog & Storage                       |
|  • Schema & Index Metadata     • Slotted-Page Record Store              |
|  • Primary B+ Tree Index       • Multi-Page Overflow Chains             |
|  • Secondary & Covering Indexes• Persistent LIFO Freelist Space Reuse   |
+-------------------------------------------------------------------------+
                                     │
                                     ▼
+-------------------------------------------------------------------------+
|                           Buffer Pool Manager                           |
|  • Frame Table Lookup          • Clock Cache Eviction Policy            |
|  • Pin / Unpin Lifecycle       • Write-Ahead Rule Enforcement           |
+-------------------------------------------------------------------------+
                                     │
                    ┌────────────────┴────────────────┐
                    │ (PageLSN <= FlushedWAL_LSN)     │
                    ▼                                 ▼
+---------------------------------------+ +-------------------------------+
|              WAL Manager              | |          Page Manager         |
|  • Physiological Redo/Undo Logging    | |  • 4,096-Byte Slotted Pages   |
|  • 3-Phase ARIES Crash Recovery       | |  • Page 0 Database Header     |
|  • Compensation Log Records (CLRs)    | |  • Synchronous Disk I/O       |
|  • Fuzzy Checkpointing & Truncation   | |  • File Sync (sync_all)       |
+---------------------------------------+ +-------------------------------+
                    │                                 │
                    ▼                                 ▼
       Write-Ahead Log (.treedb.wal)          Database File (.treedb)
```

---

## Core Systems Capabilities

* **100% Safe Rust**: `#![forbid(unsafe_code)]` is enforced across all 91 test suites and the engine crate. Memory safety is guaranteed by Rust's type system and ownership model rather than runtime pointer manipulation.
* **Slotted-Page Architecture**: 4,096-byte pages with explicit little-endian binary serialization. Supports variable-length records, local compaction, and chained overflow pages for multi-page tuples.
* **B+ Tree Index Engine**: Primary index mapping `Key = u64` to `RecordId`s with dynamic node splits, borrows, and merges. Persistent secondary indexes support unique constraints, composite multi-column keys, and covering indexes (`INCLUDE`) for index-only scans.
* **Buffer Pool & Cache**: Clock eviction policy, frame pinning, and strict enforcement of the write-ahead rule ($\text{PageLSN} \le \text{FlushedWAL\_LSN}$) before dirty frames reach disk.
* **MVCC Snapshot Isolation**: In-page version chains with monotonic `CommitSeq` identifiers prevent dirty reads, non-repeatable reads, and phantom anomalies. Online background vacuuming safely prunes dead versions behind the active snapshot horizon.
* **ARIES-Style Crash Recovery**: 3-phase recovery (Analysis, Redo from $\min(\text{recLSN})$, Undo of loser transactions via CLRs) guaranteeing recovery idempotence and resilience against crash-truncated log tails.
* **Cost-Based SQL Optimizer**: Memo-based plan exploration, single- and multi-column histogram statistics, join reordering (subset DP and greedy heuristic), index access path selection, and plan caching.
* **Bounded External Execution**: Spill-to-disk external sorting, partitioned hash aggregation, and partitioned hash joins when memory budgets are exceeded.
* **Cross-Layer Consistency Auditor**: Non-destructive structural and logical audit tool (`verify_consistency`) that verifies page allocations, freelists, index references, and version chains.

---

## Release Status

* **Package Version**: `0.1.0` (Staged locally as candidate `v0.1.0-rc2`)
* **Base RC Commit**: `e8876f20783ad7521ea027650c716bf9d4328efd`
* **Automated Test Matrix**: **2,105 passing tests** across 91 test-bearing binaries (48 library unit tests + 2,057 integration tests; 0 failures, 0 warnings, 0 ignored).
* **Quality Gates**: Strict Clippy (`-D warnings`), clean code formatting (`cargo fmt`), clean documentation (`cargo doc`), and hermetic cargo packaging (`cargo package`).
* **Registry Status**: Unpublished internal release candidate; stable crates.io release reserved.

---

## Getting Started

### Prerequisites
* **Rust Toolchain**: Stable Rust 1.80+ (`rustc` and `cargo`).

### Building
```bash
cargo build --release
```

### Running Tests
Execute the full regression suite using the repository's established test command:
```bash
cargo test -j 2
```

### Running the Interactive CLI
The included `treedb` CLI binary allows inspecting and manipulating database files:
```bash
# Initialize a new database file
cargo run --bin treedb -- init mydb.treedb

# Inspect database file headers, page counts, and freelist state
cargo run --bin treedb -- info mydb.treedb

# Execute SQL DDL and DML statements
cargo run --bin treedb -- query mydb.treedb "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);"
cargo run --bin treedb -- query mydb.treedb "INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25);"
cargo run --bin treedb -- query mydb.treedb "SELECT name, age FROM users WHERE age >= 21 ORDER BY age DESC;"
```

### Running Compile-Verified Examples
The repository includes complete, runnable examples in the [`examples/`](examples) directory:

```bash
# 1. Low-level Key-Value storage, persistence, and consistency checks
cargo run --example 01_kv_storage

# 2. Relational SQL DDL, secondary indexes, filtering, joins, and aggregates
cargo run --example 02_sql_relational

# 3. ACID transactions, multi-statement rollback with CLRs, and read-your-writes
cargo run --example 03_transactions_acid

# 4. Cost-based optimizer, EXPLAIN, EXPLAIN ANALYZE, and execution telemetry
cargo run --example 04_query_optimizer_and_metrics
```

---

## Reproducible Benchmarking

TreeDB includes a dedicated benchmark suite measuring micro-benchmarks, subsystem throughput, and scaling baselines.

```bash
# Display benchmark CLI options and categories
cargo bench --bench benchmark_suite -- --help

# Run B+ Tree indexing benchmarks (inserts, lookups, deletes across 1k, 10k, 50k keys)
cargo bench --bench benchmark_suite -- --category bplus_tree

# Run Buffer Pool caching and eviction benchmarks
cargo bench --bench benchmark_suite -- --category buffer

# Run Record Store inline and overflow read/write benchmarks
cargo bench --bench benchmark_suite -- --category record

# Run WAL and Transaction commit / recovery benchmarks
cargo bench --bench benchmark_suite -- --category wal

# Run SQL execution and query optimizer benchmarks
cargo bench --bench benchmark_suite -- --category sql

# Run dataset scale benchmarks (e.g. 2,000 or 20,000 keys) and export results to CSV
cargo bench --bench benchmark_suite -- --category scale --scale 2000 --csv target/benchmarks.csv
```

---

## Measured Performance Baselines

The following baselines were measured on an Intel Core i7-1355U (NVMe SSD, NTFS, Windows 11, `--profile bench`) as documented in [docs/PERFORMANCE_BASELINE.md](docs/PERFORMANCE_BASELINE.md):

| Subsystem | Workload / Operation | Scale | Throughput | Avg Latency | p99 Latency |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Buffer Pool** | Warm Cache Page Hit | 2,000 pages | **19,219,681 ops/s** | 52.0 ns | 100.0 ns |
| **Buffer Pool** | Cold Cache Disk Read (I/O) | 2,000 pages | 33,703 ops/s | 29.67 µs | 299.1 µs |
| **B+ Tree** | In-Memory Point Lookup | 10,000 keys | **1,260,104 ops/s** | 793.6 ns | 1.80 µs |
| **B+ Tree** | Insert into Pre-populated Tree | 50,000 keys | 430,867 ops/s | 2.32 µs | 9.20 µs |
| **Record Store** | 64B Record Read | 5,000 recs | **2,131,105 ops/s** | 469.2 ns | 1.20 µs |
| **Record Store** | 8KB Overflow Record Read | 500 recs | 49,120 ops/s | 20.36 µs | 61.40 µs |
| **WAL & Txn** | Single-Row Write Transaction | 200 rows | 2,266 txns/s | 441.3 µs | 1.03 ms |
| **WAL & Txn** | Storage Durability Sync (`sync_all`)| 50 calls | 3,412 syncs/s | 293.1 µs | 752.5 µs |
| **WAL & Txn** | ARIES Crash Recovery (1,000 logs) | 1,000 logs | 106.7 recov/s | 9.37 ms | 9.37 ms |
| **SQL Engine** | PK Point Lookup | 2,000 rows | 2,395 queries/s | 417.6 µs | 901.0 µs |
| **SQL Engine** | Plan Cache Hit Execution | 2,000 rows | **38,728 queries/s** | 25.8 µs | 48.4 µs |
| **SQL Engine** | In-Memory Hash Join ($2000 \bowtie 4000$)| 4,000 rows | 28.4 queries/s | 35.23 ms | 55.29 ms |
| **MVCC & Vac** | Version Chain Read (depth=5) | 500 rows | 3,236 queries/s | 309.0 µs | 610.9 µs |
| **MVCC & Vac** | Online Vacuum (2,500 versions) | 500 rows | 11.9 vacuums/s | 83.93 ms | 83.93 ms |

---

## Minimal Public API Usage

### 1. Key-Value Storage API
```rust
use treedb::{TreeDb, Result};

fn main() -> Result<()> {
    // Open or create a database with a 64-page buffer pool (256 KiB cache)
    let mut db = TreeDb::create("mydb.treedb", 64)?;

    // Insert, update, and lookup byte payloads mapped to 64-bit keys
    db.put(42, b"Hello, TreeDB!")?;
    if let Some(val) = db.get(42)? {
        println!("Value: {}", String::from_utf8_lossy(&val));
    }

    // Explicitly sync WAL and buffer pool to disk
    db.sync()?;
    Ok(())
}
```

### 2. Relational SQL & ACID Transactions API
```rust
use treedb::{TreeDb, Result};

fn main() -> Result<()> {
    let mut db = TreeDb::create("company.treedb", 128)?;

    // DDL: Create tables and indexes
    db.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, name TEXT, balance INT);")?;
    db.execute_statement("CREATE INDEX idx_name ON accounts (name);")?;

    // Multi-statement atomic transaction
    let mut txn = db.begin()?;
    txn.execute_statement("INSERT INTO accounts VALUES (1, 'Alice', 1000);")?;
    txn.execute_statement("INSERT INTO accounts VALUES (2, 'Bob',   500);")?;
    txn.commit()?; // Persisted synchronously to WAL

    // Query execution with Volcano iterator
    let res = db.execute_sql("SELECT name, balance FROM accounts WHERE balance > 600;")?;
    for row in res.rows() {
        println!("Holder: {}, Balance: ${}", row.values[0], row.values[1]);
    }

    Ok(())
}
```

---

## Operational Scope & Explicit Disclosures

1. **Single-Writer Operating Model**: Multi-reader concurrent transactions are supported via MVCC snapshots, but all write mutations (DML) and schema modifications (DDL) must serialize through a single engine instance. Concurrent multi-process access to the same file is unsupported.
2. **Single-File WAL**: The write-ahead log operates as a single `.wal` file. Prefix truncation reclaims disk space during checkpoints, but multi-segment log rotation is not present in `v0.1.0`.
3. **Drop vs. Sync Semantics**: Dropping `TreeDb` closes file handles without flushing dirty buffer-pool pages. Durability is maintained because all committed transactions are flushed and synced to the WAL; unwritten pages are replayed via ARIES Redo upon restart. For zero-replay clean shutdown, callers must explicitly execute `db.sync()?` or `db.checkpoint()?`.
4. **Hardware & Power Loss**: Relies on storage controllers honoring `File::sync_all()`. Does not implement double-write page buffering; torn sector writes caused by hardware power loss require backup restoration.

---

## Comprehensive Documentation Index

### Architecture & Internals
* [Systems Architecture & Engineering Portfolio](docs/SYSTEMS_PORTFOLIO.md) — Comprehensive technical briefing on architectural thesis, subsystems, and research value.
* [Architecture Specification](docs/ARCHITECTURE.md) — Detailed layer-by-layer architectural decomposition.
* [Design Decisions Log](docs/DESIGN_DECISIONS.md) — Architectural rationale, engineering trade-offs, and alternatives considered.
* [On-Disk Storage Format](docs/STORAGE_FORMAT.md) — Byte-level specifications for headers, slotted pages, and records.
* [B+ Tree Engine](docs/BPLUS_TREE.md) — Node splitting, rebalancing, leaf merging, and index layout.
* [Buffer Pool Manager](docs/BUFFER_POOL.md) — Frame tables, clock eviction, and pin/unpin semantics.
* [MVCC & Version Storage](docs/MVCC_STORAGE.md) — In-page version chains, commit sequences, and visibility.

### Durability, WAL & Recovery
* [Durability Contract & Crash Recovery](docs/DURABILITY_AND_RECOVERY.md) — Formal commit semantics, WAL ordering, and 3-phase ARIES recovery.
* [Transaction Atomicity & Testing](docs/TRANSACTION_ATOMICITY.md) — CLR undo chains and deterministic failure injection.
* [Cross-Layer Consistency Verification](docs/STORAGE_CONSISTENCY.md) — Structural integrity verification and freelist cycle detection.
* [Consistent Backup & Restore](docs/BACKUP_RESTORE.md) — Two-phase restore journal protocol and CRC32 manifests.

### Performance & Benchmarks
* [Performance Baseline Report](docs/PERFORMANCE_BASELINE.md) — 1,200+ lines of empirical benchmark results across all subsystems.

### Release Governance & Quality Gates
* [Release Readiness Audit](docs/RELEASE_READINESS.md) — Quality gate checklist, test counts, and artifact provenance.
* [Release Process & Handoff Guide](docs/RELEASING.md) — Candidate staging (`v0.1.0-rc2`) vs. crates.io stable publication procedures.
* [Changelog](CHANGELOG.md) — Chronological history of additions, fixes, and architectural milestones.

---

## Author & Maintainer

TreeDB is designed and maintained by **Vedaang Sharma** ([info@vedaangsharma.in](mailto:info@vedaangsharma.in)).

Inquiries regarding architecture, systems-programming research, or potential collaboration are welcome via email or through the project repository.

---

## Contributing

Contributions, discussions, and bug reports are welcome. All development takes place through [GitHub Issues](https://github.com/gtathelegend/TreeDB/issues) and [Pull Requests](https://github.com/gtathelegend/TreeDB/pulls).

### Submitting Bug Reports
When opening an issue, please provide:
1. A minimal, self-contained reproducible example (code snippet or SQL sequence).
2. Host environment details (OS, CPU architecture, filesystem type, Rust toolchain version `rustc --version`).
3. Observed versus expected behavior, including panic backtraces or error messages.

### Quality Gates & Verification
Before submitting a pull request, ensure all local validation gates pass:
```bash
# Code formatting
cargo fmt --all -- --check

# Target compilation
cargo check --all-targets

# Strict linting (warnings treated as errors)
cargo clippy --all-targets --all-features -- -D warnings

# Documentation generation
cargo doc --no-deps

# Regression test suite
cargo test -j 2
```

---

## License

Dual-licensed under either of:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

