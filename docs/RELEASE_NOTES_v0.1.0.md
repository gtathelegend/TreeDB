# TreeDB v0.1.0 Release Notes

## Overview

**TreeDB** is a persistent storage engine built from first principles in Safe Rust. It provides ACID transactions, write-ahead logging (WAL), B+ Tree primary and secondary indexing, Multi-Version Concurrency Control (MVCC) with snapshot isolation, cost-based SQL query optimization and execution, online vacuuming, cross-layer consistency verification, and crash-resilient offline backup and restore.

---

## Core Capabilities

### 1. Storage & Buffer Management
* **Paged Storage Format:** 4096-byte slotted pages with CRC32 checksum validation, page headers, slot arrays, and explicit little-endian byte layouts.
* **Buffer Pool Manager:** Configurable LRU buffer pool with dirty-page tracking, pin counts, write barriers, and write-ahead synchronization before disk flushes.
* **Freelist & Space Management:** Embedded page allocation with free page reuse and page truncation safety.
* **Overflow Record Handling:** Automatic multi-page chaining for oversized variable-length records.

### 2. Indexing
* **Primary Key B+ Tree:** Unique primary keys with node splitting, merging, redistribution, and binary search in internal and leaf nodes.
* **Secondary Indexes:** Multi-index support with unique constraints and non-unique posting lists.
* **Composite & Covering Indexes:** Multi-column index keys and covering payload support to avoid base table lookups during execution.

### 3. Transactions & Concurrency
* **ACID Transactions:** Full atomicity and rollback support for both statement-level and transaction-level mutations.
* **MVCC Architecture:** Version chains with transaction-visible snapshots, read-your-writes guarantees, and durable CommitSeq sequence monotonicity.
* **Snapshot Isolation:** Snapshot-level isolation policies preventing dirty reads, non-repeatable reads, and phantom anomalies.
* **Vacuum Engine:** Safe online vacuuming that reclaims unreachable version records and returns freed slots to the freelist without blocking active snapshots.

### 4. Query Engine & Optimizer
* **SQL Parsing & Binding:** Lexer, recursive descent parser, and schema binder supporting DDL (`CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`, `ANALYZE`), DML (`INSERT`, `UPDATE`, `DELETE`), and SQL queries (`SELECT`, `JOIN`, `GROUP BY`, `ORDER BY`, `LIMIT`, `OFFSET`, `CTE`, `WINDOW`).
* **Cost-Based Optimizer:** Memo-based rule engine with physical property enforcement, cardinality estimation, histogram/correlation statistics, runtime bloom/semi-join filters, and adaptive plan cache.
* **Execution Engine:** Pull-based Volcano iterator engine with vectorized streaming aggregates, parallel-friendly hash joins, external spill sorting, and Top-N heap optimization.

### 5. Durability, Recovery & Verification
* **Write-Ahead Logging (WAL):** Physiological logging with monotonically increasing Log Sequence Numbers (LSNs).
* **ARIES Recovery:** Comprehensive three-phase recovery (Analysis, Redo, Undo) reconstructing in-flight state and rolling back aborted transactions after unexpected crashes.
* **Fuzzy Checkpointing:** Safe periodic background checkpoints ensuring bounded recovery logs.
* **Cross-Layer Consistency Verifier:** Complete integrity audit auditing B+ Tree structural invariants, slot-to-record boundaries, index-to-table bi-directional alignment, freelist cycles, and CRC32 checksums.
* **Offline Backup & Restore:** Quiescent database image cloning with cryptographic CRC32 manifest auditing and journal-driven atomic restore publication.

---

## Reliability & Verification

TreeDB has undergone rigorous metamorphic, differential, and crash-fault certification:
* **Failure-Injection Testing:** Simulated crashes before, during, and after WAL flushes, dirty page writes, header updates, and backup publication.
* **Metamorphic Differential Testing:** Continuous cross-checking against independent in-memory relational reference models across fixed and pseudo-random seeds.
* **Full Test Suite:** 1,935 passed tests across 80 dedicated test binaries with 0 failures and 0 ignored tests.

---

## Compatibility

* **Rust Toolchain:** Stable Rust 1.80+ (verified and tested on `rustc 1.98.1 (48a229cea 2026-09-01)` on `x86_64-pc-windows-msvc`).
* **Rust Edition:** 2021 Edition.
* **Operating Systems:** Fully cross-platform across Windows (`FlushFileBuffers`), Linux, and macOS (`fsync`).
* **Dependencies:** Zero external C/native library dependencies. Built entirely in Safe Rust (`#![forbid(unsafe_code)]`).

---

## Known Operational Boundaries & Limitations

1. **Single-Writer Exclusivity:** TreeDB is a single-process storage engine. Concurrent multi-process write access without external locking is unsupported.
2. **Offline Quiescent Backups Only:** Backups must be initiated while the database is quiescent (no concurrent active write transactions). Online hot backup during mutation is not supported.
3. **Fail-Stop Integrity Model:** When corrupted page checksums or torn pages are detected, TreeDB fails loudly and safely. Heuristic corruption guessing or silent modification is explicitly omitted.
4. **Host Synchronization Contract:** TreeDB relies on host filesystem fsync / `sync_all` guarantees to maintain durability.

---

## Installation & Usage

### Adding as a Library Dependency
```toml
[dependencies]
treedb = "0.1.0"
```

### Minimal Rust Usage
```rust
use treedb::{TreeDb, Result};

fn main() -> Result<()> {
    let mut db = TreeDb::open("mydb.treedb", 64)?;
    db.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT, balance INT);")?;
    db.execute_statement("INSERT INTO accounts VALUES (1, 'Alice', 1000);")?;
    let res = db.execute_sql("SELECT owner, balance FROM accounts WHERE id = 1;")?;
    for row in res.rows() {
        println!("Owner: {}, Balance: {}", row.values[0], row.values[1]);
    }
    db.sync()?;
    let report = db.verify_consistency()?;
    assert!(report.is_healthy());
    Ok(())
}
```

### CLI Application
```bash
# Initialize database
treedb init mydb.treedb

# Execute query
treedb query mydb.treedb "SELECT * FROM accounts;"
```

---

## Release Validation Evidence

* **Release Tag:** `v0.1.0`
* **Release Candidate Tag:** `v0.1.0-rc1` (commit `22dfc2ef6647f9b4f778dd5bde91d142e0ecc867`)
* **Test Suite:** 1,935 tests passed; 0 failed; 0 ignored across 80 binaries.
* **Packaging Verification:** `cargo package` assembled and verified clean distributable (zero compiler or lint warnings).
* **Code Quality:** `cargo fmt --check` clean, `cargo clippy -- -D warnings` clean.
