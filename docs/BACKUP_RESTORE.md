# TreeDB Backup, Restore & Database Image Integrity (Phase 75)

## 1. Overview & Architecture

Phase 75 implements an offline backup, validation, and safe restore subsystem for TreeDB. The architecture guarantees that database images and their companion Write-Ahead Logs (WAL) are captured into self-contained, CRC32-protected, recoverable units without risking torn writes, silent corruptions, or accidental destruction of destination databases.

### Core Guarantees

* **Consistency**: Backups capture a stable, quiescent database snapshot. Any associated WAL required for crash recovery is bundled alongside the primary image.
* **Integrity**: Every backup artifact is validated through an explicit, versioned, CRC32-protected manifest (`treedb.manifest`), file-level CRC32 checksums, and Page 0 header invariant audits.
* **Deep Verification Without Silent Recovery**: Read-only consistency audits inspect backup images without triggering recovery side effects or mutating backup files.
* **Safe Staged Restore**: Destructive overwrite of existing destination files is strictly prevented unless explicitly authorized (`allow_overwrite = true`). Restorations use atomic two-phase staging with rollback protection to preserve existing databases if a restore fails.
* **CommitSeq & MVCC Monotonicity**: Durable `CommitSeq` values and historical MVCC version records survive backup and restore, preventing version collisions upon subsequent transaction writes.
* **Deterministic Failure Injection**: Named failure boundaries verify graceful handling and cleanup across backup creation, copy, manifest writing, publication, and restoration.

---

## 2. Public API Surface

The backup subsystem exposes dedicated configuration, report, and manifest types:

```rust
use std::path::{Path, PathBuf};
use treedb::{
    BackupConfig, BackupError, BackupManifest, BackupReport, BackupValidationReport,
    RestoreConfig, RestoreReport, StorageFailurePoint, TreeDb,
};

// 1. Online / Quiesced Backup from active TreeDb handle:
let mut db = TreeDb::open("mydb.treedb", 64)?;
let report = db.create_backup("backups/v1", &BackupConfig::default())?;

// 2. Offline Backup directly from file path:
let report = TreeDb::backup_database("mydb.treedb", "backups/v1", &BackupConfig::default())?;

// 3. Inspect backup manifest:
let manifest = TreeDb::inspect_backup_manifest("backups/v1")?;

// 4. Validate backup image integrity:
let validation = TreeDb::validate_backup("backups/v1", &BackupConfig::default())?;

// 5. Restore backup into destination:
let restore_report = TreeDb::restore_backup(
    "backups/v1",
    "restored/mydb.treedb",
    &RestoreConfig {
        allow_overwrite: false,
        ..Default::default()
    },
)?;
```

### Configuration Types

#### `BackupConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `fault_point` | `Option<StorageFailurePoint>` | `None` | Deterministic failure injection boundary for testing. |
| `checkpoint_before_backup` | `bool` | `true` | Executes a synchronous checkpoint and WAL prefix truncation before imaging. |
| `verify_image` | `bool` | `true` | Runs deep read-only cross-layer consistency verification on the staged backup image. |

#### `RestoreConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `allow_overwrite` | `bool` | `false` | When `true`, permits atomic replacement of an existing destination database and companion WAL. |
| `fault_point` | `Option<StorageFailurePoint>` | `None` | Deterministic failure injection boundary for testing. |
| `validate_before_restore` | `bool` | `true` | Completely verifies the backup manifest and checksums prior to modifying the filesystem. |
| `verify_after_restore` | `bool` | `true` | Opens the restored database and executes full consistency checks post-recovery. |
| `buffer_pool_size` | `usize` | `64` | In-memory frame capacity allocated for post-restore verification. |

---

## 3. Offline Lifecycle Preconditions

TreeDB maintains a single-writer persistence model. To ensure safety:

1. **Quiescence**: When invoking `db.create_backup()`, no transaction may be actively uncommitted (`Error::ActiveTransaction` is returned if an active transaction is open).
2. **Checkpointing**: The engine flushes all resident dirty pages to disk, appends a `Checkpoint` log record, syncs the Page 0 header with `last_checkpoint_lsn`, and truncates the WAL prefix prior to imaging.
3. **External Writers**: If running offline via `TreeDb::backup_database()`, the caller must ensure no concurrent external process is modifying the database or WAL files.
4. **Immutability of Source**: Backup creation does not modify the source database's logical data or schema.

---

## 4. Backup Manifest & Format Specification

Each backup directory contains:
* `treedb.manifest`: Binary, self-describing metadata header.
* `<db_name>`: Raw database file.
* `<wal_name>` (optional): Companion WAL file (if WAL tail records exist).

### Manifest Binary Encoding (`treedb.manifest`)

| Offset (Bytes) | Field | Type | Description |
|---|---|---|---|
| 0..8 | `magic` | `[u8; 8]` | Fixed magic string `b"TDBACKUP"`. |
| 8..12 | `backup_version` | `u32` (LE) | Manifest format version (currently `1`). |
| 12..16 | `db_format_version` | `u32` (LE) | Database format version from Page 0 header (`2`). |
| 16..20 | `page_size` | `u32` (LE) | Database page size (`4096`). |
| 20..24 | `page_count` | `u32` (LE) | Total allocated pages recorded in Page 0 header. |
| 24..32 | `db_file_size` | `u64` (LE) | Byte length of the database image. |
| 32..36 | `db_file_checksum` | `u32` (LE) | IEEE 802.3 CRC32 checksum of the database image. |
| 36..44 | `max_commit_seq` | `u64` (LE) | Durable `max_commit_seq` at backup time. |
| 44..52 | `last_checkpoint_lsn`| `u64` (LE) | LSN of the last durable checkpoint. |
| 52..60 | `created_at_utc` | `u64` (LE) | UTC epoch timestamp in seconds. |
| 60..62 | `db_name_len` | `u16` (LE) | Byte length of UTF-8 primary DB file name. |
| 62..62+N | `db_file_name` | `[u8; N]` | UTF-8 encoded primary DB file name. |
| 62+N | `has_wal` | `u8` | `1` if companion WAL is included, `0` otherwise. |
| ... | `wal_metadata` | (Variable) | Length, name, size, and CRC32 of WAL file (if `has_wal == 1`). |
| Tail (4) | `manifest_crc` | `u32` (LE) | IEEE 802.3 CRC32 checksum covering all preceding bytes. |

---

## 5. Checksum Semantics & Limitations

* **CRC32 Algorithm**: Standard IEEE 802.3 32-bit Cyclic Redundancy Check (`0xEDB8_8320` polynomial), computed using a streaming 64 KB buffer.
* **Scope**: Checksums protect against accidental disk bit-flips, network copy corruption, and file truncation.
* **Distinction**: Checksums guarantee **byte-level transfer integrity**, but do not prove logical database correctness. Logical correctness is verified via Phase 73/74 cross-layer consistency verification (`TreeDb::verify_file`).

---

## 6. Atomic Publication & Safe Staging Protocol

To prevent corrupted or half-written backups from being published:

1. **Backup Staging**:
   * Files are copied into a temporary sibling directory: `.tmp_backup_<pid>_<seq>`.
   * Files are flushed and synchronized to physical media (`sync_all`).
   * The staged directory is validated.
   * On failure, `.tmp_backup_*` is deleted.
   * On success, the staging directory is atomically renamed into the final `backup_dir`.

2. **Safe Staged Restore**:
   * Source backup is validated.
   * Restored files are written to `.tmp_restore_db_*` and `.tmp_restore_wal_*` in the destination directory.
   * Staged files are synced and checked with `TreeDb::verify_file`.
   * If replacing an existing database (`allow_overwrite = true`), existing files are renamed to `.bak_restore_*`.
   * Staged files are renamed into the target destination.
   * If any step fails, `.bak_restore_*` files are rolled back to their original names.
   * Upon successful replacement, backup `.bak_restore_*` files are removed.

---

## 7. CommitSeq & MVCC Preservation

* **`max_commit_seq` Preservation**: The Page 0 header records `max_commit_seq: u64`. The backup manifest mirrors this value.
* **Reopening & Monotonicity**: When `TreeDb::open` opens a restored database, it sets `next_commit_seq = max_commit_seq + 1`. All subsequent transaction commits allocate strictly increasing sequence numbers, preventing version collisions.
* **Snapshot Visibility**: MVCC historical versions and version chains survive backup and restore, allowing snapshot transactions to read consistent point-in-time snapshots.

---

## 8. Failure Injection Matrix

The failure-injection framework tests boundaries across backup and restore:

| Failure Boundary | Phase | Safety Invariant Verified |
|---|---|---|
| `BeforeBackupCreation` | Backup | No staging or published directory created; source intact. |
| `DuringBackupDbCopy` | Backup | Incomplete DB image cleaned up; no backup directory published. |
| `DuringBackupWalCopy` | Backup | Incomplete WAL image cleaned up; no backup directory published. |
| `DuringBackupManifestWrite` | Backup | Incomplete manifest cleaned up; no backup directory published. |
| `BeforeBackupPublication` | Backup | Validated staging cleaned up; existing backup untouched. |
| `BeforeRestoreValidation` | Restore | Source untouched; destination untouched. |
| `DuringRestoreDbCopy` | Restore | Incomplete restored DB cleaned up; destination untouched. |
| `DuringRestoreWalCopy` | Restore | Incomplete restored WAL cleaned up; destination untouched. |
| `BeforeRestorePublication` | Restore | Staged files cleaned up; existing destination 100% undamaged. |

---

## 9. Verification & Test Commands

Run the dedicated Phase 75 integration suite:

```bash
cargo test --test backup_restore
```

Run full workspace tests, formatting, and Clippy:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```
