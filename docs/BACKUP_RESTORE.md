# TreeDB Backup, Restore & Publication Hardening (Phase 76)

## 1. Overview & Architecture

Phase 76 establishes a hardened, release-ready persistence, backup, and restore subsystem for TreeDB. It resolves the two fundamental composition questions of offline backup systems:
1. **Source State Recoverability**: How backups are captured and recovered across every possible database and WAL state (including unflushed committed dirty buffer pool pages).
2. **Safe Multi-File Restore Publication**: How multi-file database/WAL restoration achieves deterministic crash atomicity across process terminations and system crashes via a durable restore journal (`RestoreJournal`).

TreeDB preserves a strict **single-writer** model with **safe Rust only**, without automatic corruption repair and without silent discard of recovery-required WAL log records.

---

## 2. Backup Source State Matrix

The following matrix documents TreeDB's guarantees across all database and Write-Ahead Log (WAL) source states:

| # | Source State | Backup Supported? | Recovery Required First? | Both DB & WAL Included? | Independently Recoverable? | Validation Findings | Error / Limitations Reported |
|---|---|---|---|---|---|---|---|
| 1 | **Cleanly Checkpointed Database** | Yes | No | DB only (WAL empty/absent) | Yes | `status: FullyHealthy`, all checks pass | None. Valid offline image. |
| 2 | **Valid WAL Containing Applied Records** | Yes | No | DB + WAL (or DB only if WAL truncated) | Yes | `status: FullyHealthy` | None. Replayed records match page LSNs. |
| 3 | **Valid WAL With Redo Needed for Committed Changes** | Yes | No | DB + WAL | Yes | `status: RecoverablePendingRecovery` (`UnappliedWalTail`) | Non-fatal info warning: WAL replay required on reopen. |
| 4 | **Durable Commit With Unflushed Dirty Pages** | Yes | No | DB + WAL | Yes | `status: RecoverablePendingRecovery` (`UnappliedWalTail`) | Image reflects pre-commit checkpoint; WAL holds redo records. Recovery engine restores durable rows upon reopen. |
| 5 | **WAL Containing Uncommitted Transaction Work** | Yes | No | DB + WAL | Yes | `status: RecoverablePendingRecovery` | Uncommitted transactions are discarded during recovery replay and never become committed-visible. |
| 6 | **WAL With Compensation Log Records (CLRs)** | Yes | No | DB + WAL | Yes | `status: RecoverablePendingRecovery` | ARIES recovery engine replays CLRs idempotently to resume or complete undo work. |
| 7 | **WAL With Interrupted Checkpoint** | Yes | No | DB + WAL | Yes | `status: RecoverablePendingRecovery` | Prior checkpoint LSN in DB Page 0 header serves as authoritative recovery boundary; interrupted checkpoint tail safely rolled forward. |
| 8 | **Truncated WAL** | Rejected | Yes | N/A | No | `ChecksumMismatch` or `WalRecordCorruption` | Error reported. Incomplete WAL records cannot be silently ignored. |
| 9 | **Invalid WAL Checksum** | Rejected | Yes | N/A | No | `ChecksumMismatch` | Corrupted WAL bytes fail verification. No backup declared complete. |
| 10 | **Database Header Invariant Failure** | Rejected | Manual inspection required | N/A | No | `CorruptHeader` | Fatal corruption detected on Page 0. Backup rejected. |

---

## 3. Database / WAL Companion Inclusion & Manifest Policy

### Inclusion Policy
1. **Single-file Database**: When the database is cleanly checkpointed and the WAL file does not exist or has zero bytes, `has_wal` is recorded as `false`. `treedb.manifest` records `wal_file_name: None`, `wal_file_size: 0`, and `wal_file_checksum: 0`.
2. **Companion WAL Inclusion**: Whenever a companion WAL exists and has byte length > 0, it is copied, hashed, and tracked in `treedb.manifest` with `has_wal: true`, along with its exact size and IEEE 802.3 CRC32 checksum.
3. **Companion WAL Naming**: During restore staging, the staged WAL is named `format!("{}.wal", staged_db.display())`. This naming convention guarantees that staged read-only consistency verification (`TreeDb::verify_file(staged_db)`) discovers and audits the companion WAL prior to publication.
4. **Disallowed States**:
   * *Missing companion WAL*: If `has_wal == true` in the manifest but the companion WAL file is absent from the backup directory, `validate_backup` rejects the backup with `BackupError::MissingFile`.
   * *Unexpected extra WAL*: If `has_wal == false` but an unexpected `.wal` file exists in the directory, directory artifact scanning rejects it with `BackupError::UnexpectedArtifact`.
   * *Mismatched WAL*: If a WAL belongs to a different database or displays sequence regressions, verification detects `CommitSequenceRegression` or `WalMonotonicityViolation`.

### Security Bounds on Manifest Parsing
* **Manifest Size Bound**: The manifest file size is strictly bounded to a maximum of **64 KB** (`65,536` bytes). Manifest files exceeding 64 KB are rejected before allocation.
* **Path Traversal Protection**: Both `db_file_name` and `wal_file_name` are bounded to a maximum of 255 bytes and must pass `is_safe_file_name`. Any file name containing path traversal sequences (`..`, `/`, `\`, null bytes, or single dots) is rejected with `BackupError::InvalidManifest`.

---

## 4. Durable Restore Publication Protocol (`RestoreJournal`)

Replacing a multi-file database/WAL pair across physical storage cannot rely on individually atomic filesystem renames alone. If a power outage or process termination occurs between renaming the database file and renaming the companion WAL, the destination could be left with a mismatched database and WAL pair.

To guarantee that a destination is **always either the old valid database or the new valid database**, TreeDB implements a durable two-phase publication protocol backed by a persistent journal: `RestoreJournal`.

### Journal Specification
* **File Location**: `<dest_parent>/<dest_name>.restore_journal`
* **Magic Bytes**: `b"TDRESJRN"` (8 bytes)
* **Version**: `1` (4 bytes)
* **Integrity**: Protected by trailing IEEE 802.3 CRC32 checksum over all serialized fields.
* **States**:
  1. `Staged (0)`: Staging files (`.tmp_restore_db_*`, `.tmp_restore_db_*.wal`) written, synchronized, and verified. Existing destination untouched.
  2. `ExistingBackedUp (1)`: Pre-existing destination files backed up to temporary `.bak_restore_*` files.
  3. `DbPublished (2)`: Staged primary database file renamed into destination path. Companion WAL publication pending.
  4. `AllPublished (3)`: Both primary database and companion WAL renamed into final destination paths. Cleanup of `.bak_*` files pending.

### Publication State Transitions & Failure Boundaries

```
[Start Restore]
       │
       ▼
[Stage Files] ──(Fault: DuringRestoreDbCopy / DuringRestoreWalCopy / DuringRestoreValidation)
       │          -> Staged files removed; destination untouched.
       ▼
[Write Journal: Staged] ──(Fault: DuringRestoreJournalSync)
       │          -> Recovery: staged files & journal deleted; destination untouched.
       ▼
[Backup Existing: ExistingBackedUp] ──(Fault: DuringRestoreDestinationBackup)
       │          -> Recovery: .bak files rolled back to dest; staged files & journal deleted.
       ▼
[Publish Primary DB: DbPublished] ──(Fault: DuringRestoreDbPublish)
       │          -> Recovery: .bak files rolled back to dest; staged files & journal deleted.
       ▼
[Publish Companion WAL: AllPublished] ──(Fault: DuringRestoreWalPublish)
       │          -> Recovery: staged WAL published forward; .bak files & journal deleted.
       ▼
[Cleanup Obsolete Artifacts] ──(Fault: DuringRestoreCleanup)
       │          -> Recovery: .bak files & journal deleted; new database active.
       ▼
[Complete]
```

### Deterministic Recovery Rules
When `TreeDb::recover_interrupted_restore(dest_db)` or `TreeDb::open(dest_db)` encounters a lingering `.restore_journal`:
* **State `Staged`**: Removes staged files, deletes journal. Destination remains intact with original valid files.
* **State `ExistingBackedUp`**: Rolls back `.bak_restore_*` files to original destination paths, removes staged files, deletes journal. Destination remains 100% undamaged.
* **State `DbPublished`**: Completes publication forward by renaming `staged_wal -> dest_wal`, deletes `.bak_restore_*` files, deletes journal. Destination completes publication deterministically.
* **State `AllPublished`**: Deletes lingering `.bak_restore_*` files, deletes journal. Destination is fully finalized.

### Publication Invariants
1. **Zero Silent Corruption**: The destination is never left in an unrecoverable mixed state.
2. **Automatic Healing**: `TreeDb::open` transparently detects and recovers any interrupted restore journal before opening.
3. **Artifact Retention**: Temporary staged files and backup files are never removed while an active restore journal is present on disk.

---

## 5. Non-Mutating Read-Only Inspection

`TreeDb::open_read_only_unrecovered` is guaranteed to be strictly non-mutating on disk:
* When inspecting a database image whose companion WAL is absent, `BufferPool::new` is initialized without a `LogManager`.
* It does **not** create or truncate a `.wal` file on disk, preserving byte-for-byte disk immutability for forensic and verification audits.

---

## 6. Verification & Test Commands

Run the comprehensive Phase 76 backup and restore hardening test suite:

```bash
cargo test --test backup_restore
```

Run all persistence subsystem test suites:

```bash
cargo test --test recovery_certification
cargo test --test storage_consistency
cargo test --test transaction_atomicity
cargo test --test mvcc_recovery
cargo test --test mvcc_conflicts
cargo test --test mvcc_reference_model
cargo test --test mvcc_vacuum
cargo test --test mvcc_indexes
```

Run full formatting, Clippy, and test suite verification:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```
