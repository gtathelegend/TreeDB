# Changelog

All notable changes to **TreeDB** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.1.0] - Proposed Initial Release / Release Candidate

### Overview
TreeDB is a persistent relational storage engine built from first principles in 100% Safe Rust (`#![forbid(unsafe_code)]`). It provides single-node relational database storage, ACID transactions with Multi-Version Concurrency Control (MVCC) snapshot isolation, Write-Ahead Logging (WAL) with ARIES-style crash recovery, fuzzy checkpoints, a cost-based SQL query optimizer, volcano iterator query execution, and crash-resilient backup/restore.

### Added

#### Storage Engine & B+ Tree Indexing
* **Slotted Page Architecture**: 4,096-byte pages (`PageId = u32`) with explicit little-endian headers, slot arrays, free space accounting, and CRC32 checksum verification.
* **Buffer Pool Manager**: Clock eviction policy, frame table lookup, pin/unpin tracking, dirty frame tracking, and strict Write-Ahead Logging enforcement prior to page flushing.
* **Freelist & Space Recycling**: Persistent LIFO freelist head on Page 0 with bounded page recycling and batched deallocation.
* **Overflow Record Storage**: Chained multi-page allocation for variable-length records exceeding inline page capacity.
* **B+ Tree Indexing**:
  * Primary index mapping `Key = u64` to `RecordId` with interior routing, leaf page splitting, root expansion, page borrowing, and leaf merging.
  * Secondary indexes supporting unique constraints and non-unique posting lists.
  * Composite multi-column indexes and covering indexes with embedded payloads for index-only scan evaluation.

#### SQL Execution & Query Optimization
* **SQL Frontend**: Lexer, recursive descent parser, and catalog schema binder supporting DDL (`CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`, `ANALYZE`), DML (`INSERT`, `UPDATE`, `DELETE`), and queries (`SELECT`, `WHERE`, `ORDER BY`, `GROUP BY`, `HAVING`, `LIMIT`, `OFFSET`, `JOIN`, `CTE`, `WINDOW`).
* **Cost-Based Optimizer**:
  * Memo-based logical and physical plan search with equivalence groups, rule matching, and cost evaluation.
  * Cardinality estimation using single-column histograms (equi-depth buckets) and multi-column correlation statistics.
  * Dynamic plan cache with LRU eviction and automatic invalidation on DDL mutations or statistics updates.
  * Runtime bloom/hash filtering and adaptive join/scan strategy selection.
* **Volcano Execution Pipeline**: Pull-based iterator execution supporting sequential scans, index scans, index-only scans, hash joins, merge joins, in-memory sorting, external spill sorting, streaming aggregates, and Top-N heap evaluation.

#### Transactions, Concurrency & Vacuuming
* **ACID Transactions**: Full statement and multi-statement atomicity with rollback support via Compensation Log Records (CLRs).
* **Multi-Version Concurrency Control (MVCC)**:
  * In-page version chains linking record updates with monotonic `CommitSeq` identifiers.
  * Read-your-writes semantics and snapshot isolation preventing dirty reads, non-repeatable reads, and phantom anomalies.
* **Online Vacuuming**: Safe background version pruning that unlinks dead row versions beyond the oldest active snapshot horizon without blocking active readers.

#### Durability, Write-Ahead Logging & ARIES Crash Recovery
* **Write-Ahead Logging (WAL)**: Physiological redo/undo logging enforcing $\text{PageLSN} \le \text{FlushedWAL\_LSN}$ before any dirty buffer frame reaches physical storage.
* **Synchronous Commit Contract**: `Transaction::commit()` flushes in-memory WAL buffers and calls `File::sync_all()` (`FlushFileBuffers` on Windows, `fsync` on POSIX) before returning success to the caller.
* **3-Phase ARIES Recovery**:
  * **Analysis**: Scans forward from checkpoint LSN to rebuild active (loser) transactions and dirty page tables.
  * **Redo**: Repeats history forward from $\min(\text{recLSN})$, replaying all page deltas and CLRs where $\text{rec.lsn} > \text{page.lsn}$.
  * **Undo**: Traverses loser transactions backward, applying before-images and appending CLRs with `undo_next_lsn` pointers, finalizing each with an `Abort` record.
* **Recovery Idempotence**: Re-running recovery on an already-recovered database performs zero page mutations and emits zero duplicate abort records.
* **Crash-Truncated Tail Resilience**: Detects partial trailing record fragments caused by abrupt crashes and safely truncates them, while rejecting mid-log corruptions with `Error::WalCorruption`.
* **Bounded Recovery Memory**: Streaming `WalReader` with a 64 KB base chunk buffer (dynamically grown up to 16 MB max record limit) and compact loser/dirty-page bookkeeping.

#### Checkpointing, Backup & Restore
* **Fuzzy Checkpointing**: Flushes dirty buffer pool pages, logs active transactions, flushes WAL, updates Page 0 header with `last_checkpoint_lsn`, and syncs the database file.
* **Safe WAL Prefix Truncation**: Reclaims obsolete WAL prefix bounded by $\min(\text{checkpoint\_lsn}, \min_{t \in \text{active}}(\text{first\_lsn}(t)))$.
* **Crash-Resilient Backup/Restore**: Quiescent file copy verified by cryptographic CRC32 manifests and atomic restore publication coordinated via `.restore_journal`.

#### Testing, Packaging & Quality Gates
* **Automated Test Suite**: 2,105 passing tests across 91 test-bearing binaries (48 library unit tests + 2,057 integration tests across 90 integration test suites) with 0 failures, 0 warnings, and 0 ignored tests.
* **Failure-Injection Matrix**: Comprehensive simulation of I/O errors and process crashes before, during, and after page flushes, WAL commits, header updates, and checkpoint truncation.
* **Differential Metamorphic Testing**: Oracles continuously validating SQL execution and storage against independent relational reference models.
* **Packaging & Distribution**: Verified Cargo package (`cargo package`) generating clean archive (`treedb-0.1.0.crate`) without development leftovers.

### Operational Boundaries & Disclosures
* **Single-Writer Operating Model**: Multi-reader concurrent snapshots are supported, but all write mutations must serialize through a single engine instance. Concurrent multi-process access is unsupported.
* **Single-File WAL**: Uses a single `.wal` file per database; multi-segment log rotation is not implemented in v0.1.0.
* **Drop vs. Sync Semantics**: Dropping `TreeDb` closes file handles without flushing buffer-pool pages. Durability is maintained via WAL Redo upon restart; explicit `db.sync()?` or `db.checkpoint()?` is required for clean non-recovery shutdown.
* **Hardware & Filesystem Assumptions**: Relies on host storage controllers honoring `File::sync_all()`. Does not implement double-write page buffering; catastrophic power loss mid-sector write requires backup restoration.
