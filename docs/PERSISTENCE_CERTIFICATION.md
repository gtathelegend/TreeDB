# TreeDB Persistence Certification & Release Candidate Audit (Phase 77)

## 1. Executive Summary

This document certifies TreeDB's persistence subsystem across its complete database lifecycle:
* Transaction commit and abort durability
* Checkpoints and Write-Ahead Log (WAL) truncation
* Crash recovery under the ARIES protocol
* MVCC version-chain integrity and snapshot visibility
* Vacuum and space reclamation
* Backup creation and image integrity
* Restore publication and journal-driven crash recovery
* Post-restore write resumption and CommitSeq monotonicity
* Cross-layer consistency verification

All claims in this document are supported by deterministic integration tests and code inspection.

---

## 2. Tested Persistence Lifecycle

The persistence lifecycle was verified through end-to-end integration sequences (`test_stage3_end_to_end_lifecycle_with_independent_model`) and failure-boundary tests:

```mermaid
stateDiagram-v2
    [*] --> ActiveDB: TreeDb::open / TreeDb::create
    ActiveDB --> TransactionActive: begin()
    TransactionActive --> ActiveDB: commit() / abort()
    ActiveDB --> CheckpointedDB: checkpoint_and_truncate_wal()
    CheckpointedDB --> VacuumedDB: vacuum()
    VacuumedDB --> BackupImage: TreeDb::backup_database()
    BackupImage --> StagedRestore: TreeDb::restore_backup() [staging]
    StagedRestore --> PublishedRestore: RestoreJournal Commit & Rename
    PublishedRestore --> ResumedDB: TreeDb::open() & New Writes
    ResumedDB --> [*]
```

At every boundary in this lifecycle:
1. Complete logical table and index contents were compared against an independent in-memory reference model (`LogicalModel`).
2. Structural invariants were audited using the cross-layer consistency verifier (`TreeDb::verify_consistency`).
3. Commit sequence monotonicity was verified.

---

## 3. Commit Durability Contract

TreeDB implements strict write-ahead logging before reporting transaction success:

1. **Durable Commit Point**:
   * A transaction is durable if and only if its `Commit` log record has been serialized, CRC32 checksummed, and flushed to disk (`LogManager::flush()` invoking `File::sync_all()`).
   * Failure injection immediately prior to the commit record flush (`StorageFailurePoint::BeforeCommitRecordLogged` / `AfterCommitRecordLoggedBeforeFlush`) guarantees that none of the transaction's mutations become committed-visible.
   * Failure injection after the commit record flush (`StorageFailurePoint::AfterDurableCommitRecordFlush`) guarantees that every mutation survives crash recovery upon reopen.
2. **Dirty Page Flushing Independence**:
   * TreeDB adheres to the steal/no-force policy: committed transaction data surviving in dirty in-memory buffer pool pages that have not yet been written to the database file are fully reconstructed by replaying redo records from the WAL during crash recovery.

---

## 4. Checkpoint & Recovery Guarantees

TreeDB's checkpoint protocol coordinates the Buffer Pool, Page Manager, and Log Manager:

1. **Fuzzy Checkpoint Sequence**:
   * Flushes all dirty buffer pool frames to disk via `File::sync_all()`.
   * Appends a `Checkpoint` log record containing active transaction table and dirty page table state.
   * Flushes the WAL to disk.
   * Atomically updates Page 0 (`DbHeader`) with the checkpoint LSN, max commit sequence, and page count.
   * Flushes the database header to disk.
   * Truncates obsolete WAL records up to the checkpoint horizon.
2. **Recovery Invariants**:
   * Analysis pass scans forward from the last checkpoint LSN to rebuild the transaction table and determine the redo start point.
   * Redo pass repeats history forward, ensuring all committed changes (including index updates and overflow allocations) are reflected in memory and storage.
   * Undo pass rolls back active uncommitted transactions in reverse order, writing Compensation Log Records (CLRs) to ensure idempotency if recovery is interrupted.
   * Monotonicity: `max_commit_seq` is restored to `max(header.max_commit_seq, recovery.max_commit_seq)`.

---

## 5. MVCC & Vacuum Invariants

1. **Version-Chain Integrity**:
   * Version records form a backwards-linked delta chain originating at the primary key slot in `RecordStore`.
   * Uncommitted versions are tagged with the active `TxnId` and are invisible to other snapshots.
   * Rollback cleanly unlinks and marks uncommitted versions as aborted; primary key and unique index slots are immediately reusable.
2. **Vacuum Reclamation**:
   * Only versions older than the oldest active snapshot horizon (`reclamation_horizon`) are eligible for pruning.
   * Dead record slots and freed overflow pages are returned to the `FreePageManager` freelist.
   * Consistency verifier confirms that reclaimed pages appear exclusively in the freelist and never in live table structures.

---

## 6. Backup Source-State Support Matrix

TreeDB defines clear operational rules for every supported offline database/WAL state:

| State # | Database & WAL Source State | Supported for Backup | Must Recover First | Include WAL in Backup | Independently Recoverable | Pre-Backup Validation |
|---|---|---|---|---|---|---|
| **1** | Cleanly checkpointed database | Yes | No | Yes (Empty/Clean) | Yes | Validates header, freelist, B+ trees, RecordStore |
| **2** | Valid WAL with records already reflected in DB | Yes | No | Yes | Yes | Validates DB and WAL CRC checksums |
| **3** | Valid WAL containing redo needed for committed changes | Yes | Recommended (not required) | **Yes (Mandatory)** | Yes | Status: `RecoverablePendingRecovery` |
| **4** | Durable commit with unflushed dirty pages | Yes | Recommended (not required) | **Yes (Mandatory)** | Yes | Redo tail verified prior to restore |
| **5** | WAL containing uncommitted transaction work | Yes | No | **Yes (Mandatory)** | Yes | Uncommitted rows rolled back on restore |
| **6** | WAL with CLRs from interrupted recovery | Yes | Recommended | **Yes (Mandatory)** | Yes | Target engine continues undo cleanly |
| **7** | WAL with interrupted checkpoint | Yes | No | **Yes (Mandatory)** | Yes | Recovers from prior valid checkpoint |
| **8** | Truncated WAL | **No** | Yes | N/A | No | Detects premature EOF / framing error |
| **9** | Invalid WAL checksum | **No** | No | N/A | No | Streaming CRC32 detects mismatch |
| **10** | DB requiring recovery without companion WAL | **No** | Yes | N/A | No | Standalone DB missing required WAL rejected |

---

## 7. Restore-Journal State-Transition Table

Multi-file restore atomicity across non-atomic operating system renames is guaranteed by `RestoreJournal` (`<dest>.restore_journal`):

| Journal State | Value | DB Files Present | WAL Files Present | Previous Dest Available? | New Dest Complete? | Recovery Action | Permitted File Removals |
|---|---|---|---|---|---|---|---|
| `Staged` | 0 | `staged_db`, `dest_db` (if existed) | `staged_wal`, `dest_wal` (if existed) | Yes (`dest_db`, `dest_wal`) | No | Roll back: delete staging files, delete journal | `staged_db`, `staged_wal`, journal |
| `ExistingBackedUp` | 1 | `staged_db`, `bak_db` (or none if fresh) | `staged_wal`, `bak_wal` (or none if fresh) | Yes (`bak_db`, `bak_wal`) | No | Roll back: rename `bak_db -> dest_db`, `bak_wal -> dest_wal` (or remove `dest_db`/`dest_wal` if fresh); delete staging | `staged_db`, `staged_wal`, orphaned dest files, journal |
| `DbPublished` | 2 | `dest_db` (new), `bak_db` | `staged_wal` (or `dest_wal`), `bak_wal` | Yes (`bak_db`, `bak_wal`) | Partial (DB only) | Roll forward: rename `staged_wal -> dest_wal`; delete `.bak_*`. *Fallback if staged WAL missing:* roll back to `.bak_*` | `bak_db`, `bak_wal`, `staged_db`, `staged_wal`, journal |
| `AllPublished` | 3 | `dest_db` (new), `bak_db` (transient) | `dest_wal` (new), `bak_wal` (transient) | Obsolete (`bak_db`, `bak_wal`) | Yes | Complete cleanup: remove `.bak_*`, delete journal | `bak_db`, `bak_wal`, `staged_db`, `staged_wal`, journal |

### Invariants Certified:
1. **No Destructive Cleanup on Corrupt Journal**: If a journal buffer fails CRC32 or magic validation, `recover_journal` returns `Err(BackupError::ChecksumMismatch)` or `Err(BackupError::InvalidManifest)` and deletes zero files.
2. **No Data Loss on Missing Staged WAL**: If roll-forward in `DbPublished` cannot locate `staged_wal` and `dest_wal` is absent, recovery automatically falls back to restoring `bak_db` and `bak_wal` rather than deleting the original valid database.
3. **Orphan Cleanup on Fresh Destinations**: If publication fails into a fresh destination that did not previously exist, rollback removes the half-published `dest_db` and leaves zero orphaned database files.
4. **Idempotency**: Executing `TreeDb::recover_interrupted_restore` multiple times in succession produces `Ok(true)` on the first pass and `Ok(false)` on subsequent passes without altering disk state.

---

## 8. Failure-Injection Boundaries Exercised

The following deterministic failure boundaries were exercised and verified in integration tests:

| Boundary Variant | Stage | Target Under Test | Invariant Verified |
|---|---|---|---|
| `BeforeBackupCreation` | Backup | Source database | No temporary directory created; source remains operational. |
| `DuringBackupDbCopy` | Backup | Image copy | Incomplete image deleted; zero file leakage. |
| `DuringBackupWalCopy` | Backup | WAL copy | Incomplete WAL image deleted; zero file leakage. |
| `DuringBackupManifestWrite` | Backup | Manifest creation | Backup directory deleted; zero incomplete artifacts. |
| `DuringBackupValidation` | Backup | Manifest/image audit | Incomplete backup rejected; staging removed. |
| `DuringBackupSync` | Backup | fsync | Incomplete backup removed; source untouched. |
| `BeforeBackupPublication` | Backup | Publication rename | Staging cleaned up; existing backup untouched. |
| `BeforeRestoreValidation` | Restore | Image validation | Destination untouched. |
| `DuringRestoreDbCopy` | Restore | Staged DB copy | Staged files removed; destination untouched. |
| `DuringRestoreWalCopy` | Restore | Staged WAL copy | Staged files removed; destination untouched. |
| `DuringRestoreValidation` | Restore | Staged verification | Restored files rejected before touching destination. |
| `DuringRestoreJournalSync` | Restore | Journal write | Staged files removed; destination untouched. |
| `DuringRestoreDestinationBackup`| Restore | Old destination move | Rolled back from `.bak_*`; destination operational. |
| `DuringRestoreDbPublish` | Restore | DB publication rename | Rolled back to original database; destination operational. |
| `DuringRestoreWalPublish` | Restore | WAL publication rename | Rolled forward; companion WAL published; database valid. |
| `DuringRestoreCleanup` | Restore | Post-publish cleanup | Temporary `.bak_*` files removed; database valid. |

---

## 9. Independent Logical-State Comparison Methodology

To prevent circular reasoning (where a database test scans the very B+ Tree or index implementation being verified), tests employ an independent reference model (`LogicalModel`):
* Implemented as an in-memory `BTreeMap<i64, ExpectedAccount>` tracking logical row states.
* Maintains explicit state transitions on `insert`, `update`, and `delete`.
* Verifies complete row fields (`id`, `account_num`, `balance`, `category`, `payload`).
* Validates through three distinct access mechanisms:
  1. Full table scan (`tbl.scan(None, None)`).
  2. Point lookup via Primary Key (`tbl.get(&Value::Int64(id))`).
  3. SQL execution engine evaluation (`SELECT count(*) FROM accounts`).

---

## 10. Filesystem & Synchronization Assumptions

1. **POSIX / Windows Synchronization**:
   * TreeDB calls `File::sync_all()` on every data page write, WAL flush, manifest write, and journal update.
   * Crash durability assumes the underlying storage hardware and OS filesystem flush data blocks and inode/metadata updates on `fsync` / `FlushFileBuffers`.
2. **Directory Synchronization**:
   * On filesystems where directory metadata updates are non-journaled, directory rename atomicity depends on OS filesystem semantics.
3. **Single-Writer Exclusivity**:
   * TreeDB relies on external process coordination for exclusive database access. Concurrent multi-process access to the same database file without coordination is unsupported.

---

## 11. Known Operational Limitations

1. **Offline Backups Only**: Backup APIs require quiescence (no active write transactions). Online hot backups under concurrent mutations are not supported.
2. **No Heuristic Auto-Repair**: TreeDB does not perform silent or heuristic repair of damaged data pages. Corrupted headers or checksum mismatches are rejected immediately to protect data integrity.
3. **Bounded Manifests**: Backup manifests are bounded at 64 KB, and file names must be safe ASCII/UTF-8 identifiers not exceeding 255 bytes without path traversal.

---

## 12. Reproduction Commands

All certification tests are deterministic and reproducible using standard cargo commands:

```bash
# Focused backup and restore certification suite (48 tests)
cargo test --test backup_restore

# Focused persistence certification suites
cargo test --test recovery_certification
cargo test --test storage_consistency
cargo test --test transaction_atomicity
cargo test --test mvcc_recovery
cargo test --test mvcc_conflicts
cargo test --test mvcc_reference_model
cargo test --test mvcc_vacuum
cargo test --test mvcc_indexes

# Full test suite validation
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

---

## 13. Audit Sign-Off

* **Language**: Safe Rust only (`#![forbid(unsafe_code)]` or zero `unsafe` usage).
* **Defects Identified & Resolved in Phase 77**: 6 confirmed defects (DEF-77-01 through DEF-77-06).
* **Test Count**: 1,932 tests passing across 80 test binaries with 0 failures, 0 ignored, and 0 filtered out.
* **Subsystem Status**: Persistence subsystem certified for Release Candidate status.
