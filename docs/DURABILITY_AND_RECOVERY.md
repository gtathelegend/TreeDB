# TreeDB Durability Contract & Crash Recovery Specification

## Executive Summary

This document defines the formal, evidence-based durability contract and crash-recovery semantics for **TreeDB v0.1.0**. TreeDB is a single-node, persistent relational storage engine built from first principles in Rust (`#![forbid(unsafe_code)]`).

Every guarantee stated herein is classified into one of four empirical tiers:
1. **Implemented Behavior**: Directly established by source code inspection.
2. **Tested Behavior**: Validated by automated, deterministic regression test suites.
3. **Environmental Assumptions**: Dependent on operating system, filesystem semantics, and storage device hardware.
4. **Known Limitations**: Explicit operational boundaries, architectural constraints, and failure modes outside the current release scope.

---

## 1. The Transaction Commit Contract

### 1.1 Commit Semantics
A call to `Transaction::commit()` (`src/engine/transaction.rs`) returns `Ok(())` if and only if the transaction's commit record has been serialized, appended to the Write-Ahead Log (WAL), written to the underlying operating system file, and synchronized to physical media via `File::sync_all()`.

* **Implemented Behavior**:
  * In `Transaction::commit()`, MVCC row version headers and page modifications are applied to buffer pool frames while `active_txn` remains registered.
  * In `LogManager::commit_txn_with_seq_and_fault()` (`src/wal/log_manager.rs`), a `LogRecord::Commit` (or `CommitWithSeq`) is appended to the WAL buffer with a new monotonically increasing `Lsn`.
  * The log manager calls `self.flush(commit_lsn)?`, which writes all in-memory WAL buffer segments to the filesystem and calls `self.file.sync_all()?`.
  * Only after `sync_all()` completes with `Ok(())` is the transaction removed from the active transaction table and `Ok(())` returned to the caller.
* **Tested Behavior**:
  * `tests/wal_crash_recovery_certification.rs:test_committed_transaction_persists_across_reopen`: Verifies that committed transactions persist across database reopen.
  * `tests/transaction_atomicity.rs:test_commit_records_are_flushed_before_return`: Confirms commit returns only after WAL flush.
  * `tests/persistence_failpoints.rs`: Verifies that simulated I/O errors during commit flush propagate `Err` to caller and prevent false commit acknowledgement.
* **Environmental Assumptions**:
  * The OS kernel and storage controller respect `sync_all` (`FlushFileBuffers` on Windows, `fsync`/`fdatasync` on POSIX) and do not return success while data resides solely in volatile drive cache.
* **Known Limitations**:
  * Group commit is not implemented. Each write transaction commit triggers an individual synchronous I/O barrier.

---

## 2. Write-Ahead Logging & Ordering Rules

### 2.1 The Write-Ahead Rule (PageLSN <= Flushed LSN)
TreeDB strictly enforces the foundational Write-Ahead Logging (WAL) invariant:
$$\text{PageLSN} \le \text{FlushedWAL\_LSN}$$
No dirty database page frame from the buffer pool may be written to the database file on disk until all log records describing modifications up to and including that page's `PageLSN` have been flushed and synced to disk in the WAL.

* **Implemented Behavior**:
  * In `BufferPool::flush_frame_internal()` (`src/storage/buffer_pool.rs`), before `PageManager::write_page_at(page_id, page)` is called, the buffer pool executes:
    ```rust
    if let Some(ref mut wal) = self.wal {
        wal.flush(page.lsn())?;
    }
    ```
  * In `BufferPool::get_free_frame()`, eviction of a dirty victim frame under cache pressure similarly invokes `flush_frame_internal()`, forcing the requisite WAL records to disk prior to overwriting or writing the database page.
* **Tested Behavior**:
  * `tests/buffer_pool.rs:test_buffer_pool_eviction_forces_wal_flush`: Validates that dirty page eviction forces WAL flush past PageLSN.
  * `tests/post_recovery_storage_invariants.rs:test_buffer_pool_dirty_page_flushing_preserves_page_lsn`: Asserts that page LSNs remain strictly synchronized with flushed WAL state.
* **Environmental Assumptions**:
  * File writes to the database file and WAL file are isolated and do not reorder across explicit synchronization barriers.
* **Known Limitations**:
  * TreeDB utilizes a single `.wal` file per database instance. Multi-segment log rotation is not supported in `v0.1.0`.

---

## 3. Flush and Synchronization Operations

The table below catalogs every operation in TreeDB that performs a flush or sync:

| Operation | Component | File Affected | Mechanism | Error Propagation |
| :--- | :--- | :--- | :--- | :--- |
| `Transaction::commit()` | `LogManager` | `<dbname>.wal` | `write_all()` + `sync_all()` | `Err` returned immediately; active transaction remains uncommitted. |
| `BufferPool::sync()` / `TreeDb::sync()` | `BufferPool`, `PageManager` | `<dbname>`, `<dbname>.wal` | `flush_all()` + `wal.flush()` + `pm.sync()` | Fails if any page write, WAL flush, or DB file sync fails. |
| `BufferPool::checkpoint()` | `BufferPool`, `PageManager`, `LogManager` | `<dbname>`, `<dbname>.wal` | `flush_all()` + `wal.append_checkpoint()` + `wal.flush()` + `pm.sync()` | Checkpoint aborted on failure; Page 0 header untouched if WAL flush fails. |
| Eviction of Dirty Frame | `BufferPool` | `<dbname>`, `<dbname>.wal` | `wal.flush(page.lsn())` + `write_page_at()` | Allocation/fetch fails; frame remains pinned/dirty in pool. |
| Database Header Update | `PageManager` | `<dbname>` | Header page overwrite + `sync_all()` | Returns `Err(Error::Io)`; prevents corrupt header state. |
| `TreeDb::drop()` | Standard Library | OS handles | Closed upon `File` descriptor drop | **No automatic page flush or sync on drop.** |

* **Critical Architectural Detail: Database Close / Drop Behavior**:
  * In TreeDB `v0.1.0`, `TreeDb`, `BufferPool`, and `PageManager` intentionally do **not** implement custom `Drop` logic that flushes dirty pages.
  * If a user drops `TreeDb` without calling `db.sync()` or `db.checkpoint()`, dirty pages in the buffer pool are **not** written to the database file.
  * **Durability Guarantee Under Drop**: Because every committed transaction already had its `PageDelta` and `Commit` records flushed and synced to `<dbname>.wal`, any unwritten page modifications are completely reconstructed via ARIES Redo recovery upon subsequent `TreeDb::open()`.
  * If graceful, clean shutdown without startup recovery replay is desired, callers must explicitly call `db.sync()?` or `db.checkpoint()?` prior to dropping the database instance.

---

## 4. Checkpointing & WAL Prefix Truncation

### 4.1 Checkpoint Sequence
`TreeDb::checkpoint()` executes a fuzzy checkpoint using the following atomic progression:
1. **Dirty Page Flush**: Flushes all resident dirty frames in the buffer pool to disk via `PageManager`, enforcing the Write-Ahead rule for each page.
2. **Active Transaction Snapshot**: Queries `LogManager::active_txns()` to capture active uncommitted transactions and their first LSNs.
3. **Checkpoint Record Append & Flush**: Appends a `LogRecord::Checkpoint` containing the active transaction list, and immediately flushes the WAL through the checkpoint record's LSN.
4. **Header Checkpoint LSN Update**: Writes the new `checkpoint_lsn` to the database header on Page 0 and calls `page_manager.sync()`.

### 4.2 Safe Prefix Truncation Boundary
`TreeDb::checkpoint_and_truncate_wal()` computes the retention boundary:
$$\text{retention\_lsn} = \min\left(\text{last\_checkpoint\_lsn}, \min_{t \in \text{active\_txns}}(\text{first\_lsn}(t))\right)$$
* Any log record prior to `retention_lsn` is strictly obsolete: all pages it modified are durable on disk, and no active transaction requires it for rollback.
* The WAL file prefix prior to `retention_lsn` is truncated, reclaiming disk space without jeopardizing recovery.

* **Implemented Behavior**:
  * Enforced in `BufferPool::checkpoint_and_truncate_wal()` (`src/storage/buffer_pool.rs`) and `LogManager::calculate_retention_lsn()` (`src/wal/log_manager.rs`).
* **Tested Behavior**:
  * `tests/checkpoint.rs`: Validates checkpoint record generation, dirty page flushing, and restart from checkpoint LSN.
  * `tests/wal_bounded_recovery.rs`: Verifies that recovery bounded by checkpoint LSN ignores truncated log records and recovers active transactions correctly.
* **Environmental Assumptions**:
  * Atomic file replacement or file rewriting does not corrupt file metadata during prefix truncation.
* **Known Limitations**:
  * Long-running uncommitted transactions prevent WAL reclamation because `retention_lsn` cannot advance past the oldest active transaction's first LSN.

---

## 5. Startup Crash Recovery (ARIES)

When opening an existing database via `TreeDb::open(path, pool_capacity)`, TreeDB executes a 3-phase ARIES recovery algorithm before opening the buffer pool:

```
[Analysis Phase]
 Scan forward from Checkpoint LSN (or WAL start)
 ├── Identify committed vs uncommitted (loser) transactions
 └── Reconstruct Dirty Page Table (recLSN per page)
        │
        ▼
[Redo Phase]
 Repeat History forward from min(recLSN)
 ├── Replay all PageDelta and CLR records where rec.lsn > page.lsn()
 └── Ensure durable storage reflects all committed modifications
        │
        ▼
[Undo Phase]
 Scan backward reversing loser transactions
 ├── Undo loser PageDeltas in reverse LSN order
 ├── Emit Compensation Log Records (CLRs) with undo_next_lsn
 └── Emit final Abort record once a loser's undo chain is exhausted
```

### 5.1 Loser Transaction Finalization & CLR Chains
* During the Undo phase, each undone modification emits a Compensation Log Record (`CLR`).
* A CLR points to the previous undo target via `undo_next_lsn`. CLRs are **never undone**, preventing infinite undo loops across crashes during recovery.
* When a loser transaction has undone all its operations, an `Abort` log record is appended to the WAL and flushed, transitioning the transaction to fully finalized.

### 5.2 Repeated Recovery Idempotency
TreeDB guarantees that startup recovery is strictly **idempotent**:
* If recovery is interrupted by a crash at any point, subsequent recovery resumes cleanly.
* Re-running recovery on an already-recovered database performs 0 page modifications, identifies 0 loser transactions, and appends 0 duplicate `Abort` records.

* **Implemented Behavior**:
  * `RecoveryEngine::recover()` (`src/wal/recovery.rs`) coordinates analysis, redo, and undo passes.
  * Page LSN checks (`if rec.lsn <= page.lsn() { continue; }`) guarantee redo idempotency.
* **Tested Behavior**:
  * `tests/post_recovery_storage_invariants.rs:test_recovery_finalization_exact_single_abort_record`: Validates exactly one Abort record emitted per loser transaction.
  * `tests/post_recovery_storage_invariants.rs:test_recovery_no_duplicate_abort_on_subsequent_recoveries`: Validates 3 consecutive recovery cycles produce 0 duplicate abort records.
  * `tests/post_recovery_storage_invariants.rs:test_recovery_pending_undo_not_finalized_prematurely`: Verifies that crash during undo leaves remaining operations intact for next recovery.
  * `tests/wal_crash_recovery_certification.rs:test_repeated_crash_recovery_idempotence`: Runs multi-cycle crash-recovery loops over complex workloads.
* **Environmental Assumptions**:
  * Memory allocated during recovery is available and not exhausted by excessive uncommitted transactions.
* **Known Limitations**:
  * Analysis phase must scan from checkpoint LSN to EOF; very large gap between checkpoint and EOF increases recovery duration.

---

## 6. Trailing Partial Records vs. Mid-Log Corruption

TreeDB distinguishes between **crash-truncated trailing fragments** and **durable mid-log corruption**:

### 6.1 Trailing Incomplete Records (Benign Crash Fragment)
* If the process crashes while writing a log record, or if power fails before a trailing write is synced, an incomplete record header or truncated payload may remain at the very end of the WAL file.
* `WalReader` detects unexpected EOF or length prefix exceeding available trailing bytes.
* If and only if the invalid record is at the trailing edge of the log, TreeDB treats the fragment as an uncommitted trailing write, truncates the partial tail, and completes recovery of all prior committed records.

### 6.2 Mid-Log Corruption (Fatal Data Integrity Violation)
* If an invalid record header, non-monotonic LSN, or CRC32 checksum mismatch occurs **before** valid trailing records, this indicates media degradation, bit rot, or disk block corruption.
* TreeDB **never** silently ignores mid-log corruption. Recovery halts immediately and returns `Error::WalCorruption`.
* `TreeDb::open()` fails, preventing execution on compromised storage.

* **Implemented Behavior**:
  * `WalReader::next_record()` (`src/wal/reader.rs`) distinguishes between EOF at trailing boundary and payload corruption.
* **Tested Behavior**:
  * `tests/wal_bounded_recovery.rs:test_recovery_truncated_final_wal_record`: Verifies graceful recovery when trailing record is truncated.
  * `tests/wal_bounded_recovery.rs:test_recovery_rejects_corruption_before_final_record`: Confirms `Error::WalCorruption` returned when corruption precedes valid records.
  * `tests/wal_offset_and_recovery_audit.rs:test_wal_trailing_partial_write_accepted_if_unflushed`: Validates partial trailing writes are accepted and sanitized.
* **Environmental Assumptions**:
  * CRC32 accurately detects single-bit and multi-bit data corruption in log frames.
* **Known Limitations**:
  * Online automatic repair of corrupt mid-log pages is not supported; manual restore from backup is required.

---

## 7. Operational Boundaries & Environmental Assumptions

### 7.1 Single-Writer Operating Model
* TreeDB operates as a single-writer engine. Multiple reader transactions can execute concurrently via MVCC snapshot isolation, but all schema changes and write transactions must serialize through the single database engine instance.
* Multi-process access to the same database file is prohibited and will cause corruption if write locks are bypassed.

### 7.2 Filesystem & Hardware Assumptions
TreeDB's durability guarantees hold under standard POSIX / Win32 storage contracts:
1. **Sync Semantics**: `File::sync_all()` flushes both file data and filesystem metadata to non-volatile storage.
2. **Atomic Sector Writes**: Disk sectors (or 4KB OS pages) are written atomically by the hardware.
3. **Monotonic Clocks & Timestamps**: System time does not move backward across transaction sequences.

### 7.3 What is NOT Guaranteed
TreeDB explicitly does **not** guarantee durability or recovery under:
* Storage devices or drive controllers that ignore `FlushFileBuffers` / `fsync` or return write acknowledgement before data is durable.
* Torn page writes caused by catastrophic hardware failure where the storage medium writes partial 4KB blocks without battery-backed write cache.
* Operating system kernel panic or power failure occurring while non-journaled filesystem metadata is in an inconsistent state.
* Manual tampering, truncation, or concurrent modification of `<dbname>` or `<dbname>.wal` files by external processes.
