# Recovery Certification, Checkpoint Integrity & Crash-State Verification

## 1. Executive Summary

TreeDB Phase 74 certifies the correctness, durability, and recovery invariants of the persistent storage engine across crash and restart boundaries. Building upon the Phase 73 cross-layer consistency verifier and Phase 72 failure-injection framework, Phase 74 establishes through deterministic testing that:
1. **Durable Commits are Inviolable**: Once a transaction's commit log record has been flushed and fsynced to disk, its logical mutations are permanently durable and restored by recovery even if dirty memory frames never reached the database file before crash.
2. **Uncommitted Writes Never Leak**: Active, uncommitted, or aborted transactions are cleanly rolled back during ARIES undo pass; their entries in primary, secondary, composite, unique, and covering indexes are reverted, version chains are cut/restored, and unique keys are immediately reusable without constraint collisions.
3. **Commit Sequences Advance Monotonically**: `CommitSeq` values are durable across crashes, surviving repeated reopen and recovery cycles without regressing or colliding.
4. **Historical MVCC Snapshots Remain Accessible**: Historical multi-version record chains preserved across crash recovery remain navigable by earlier active snapshot horizons.
5. **Checkpoint Integrity Holds Across All 8 Persistence Boundaries**: Crashes before, during, or after checkpoint initiation, dirty-page flushing, checkpoint WAL logging, Page 0 header persistence, and WAL truncation are verified to recover cleanly with zero data loss or structural corruption.
6. **Cross-Layer Invariants Certified via Phase 73 Verifier**: The Phase 73 verifier serves as an independent postcondition oracle, validating that recovered databases maintain healthy page ownership, freelist cycle freedom, index-record equivalence, and WAL decodability without performing silent modifications.

---

## 2. Persistence Protocol & Commit Durability Contract

### 2.1 The Durable Commit Point
TreeDB implements Write-Ahead Logging (WAL) using ARIES protocol semantics:
- When a transaction commits, `LogManager::commit_txn_with_seq_and_fault` writes a `LogRecordType::Commit` record containing transaction ID, previous LSN, and assigned `CommitSeq`.
- **Exact Durable Commit Point**: The transaction becomes durable at the instant `LogManager::flush(commit_lsn)` completes, syncing the Commit log record and its CRC32 checksum to physical storage (`File::sync_data` / `File::sync_all`).
- **Pre-Commit Crash Behavior**: If an abrupt crash occurs before this flush completes, the transaction is classified as an uncommitted loser transaction during the Analysis pass and rolled back by the Undo pass.
- **Post-Commit Crash Behavior**: If an abrupt crash occurs at any point after this flush completes, the transaction is classified as committed during the Analysis pass, and all of its modifications are redone during the Redo pass.

### 2.2 Relationship Between WAL Durability and Dirty-Page Flushing
TreeDB strictly enforces the Write-Ahead Logging rule:
- `BufferPool::flush_frame_internal` checks the dirty page's `PageLSN` against `wal.flushed_lsn()`.
- If `page.lsn() > wal.flushed_lsn()`, the WAL is flushed through `page.lsn()` *before* the dirty page is allowed to be written to disk via `PageManager`.
- This guarantees that no page containing un-logged or un-flushed modifications can ever reach the persistent database file ahead of its corresponding log records.

### 2.3 Checkpoint Boundary and WAL Retention
TreeDB executes synchronous checkpoints via `BufferPool::checkpoint` and `BufferPool::checkpoint_and_truncate_wal`:
1. `flush_all()` flushes all in-memory dirty buffer frames to disk, enforcing `PageLSN <= durable WAL LSN`.
2. Active uncommitted transactions are collected (`wal.active_txns()`).
3. A `LogRecordType::Checkpoint` record is appended and flushed to the WAL.
4. The database header on Page 0 is updated with `last_checkpoint_lsn` and synced to disk via `PageManager::sync()`.
5. `calculate_retention_lsn(checkpoint_lsn)` calculates `min(checkpoint_lsn, min(first_lsn_of_active_txns))`.
6. `wal.truncate_prefix(retention_lsn)` safely truncates obsolete log records preceding this retention boundary.
7. During crash recovery, the Analysis pass begins at `min(min_active_lsn, last_checkpoint_lsn)`, skipping already-applied history.

### 2.4 How CommitSeq Survives Restart
- During the Analysis pass, `RecoveryEngine::recover` records the maximum commit sequence observed across all `Commit` log records: `stats.max_commit_seq = max(stats.max_commit_seq, commit_seq)`.
- At the end of recovery, `page_manager.set_max_commit_seq(stats.max_commit_seq)` updates and syncs the database header on Page 0.
- When `TreeDb::open` completes, `next_commit_seq` is initialized to `max_commit_seq.saturating_add(1)`.
- This ensures that `CommitSeq` never regresses, never collides with past transactions, and continues advancing monotonically.

---

## 3. Checkpoint Failure-Injection Matrix

Phase 74 tests all 8 distinct persistence boundaries in the checkpoint lifecycle:

| Boundary | Storage Failure Point | Persistent Disk State at Crash | Recovery Action & Certified Outcome |
|---|---|---|---|
| **1. Before Initiation** | `BeforeCheckpointInitiation` | Checkpoint not started; in-memory dirty pages unwritten; header unchanged. | Recovers all committed and uncommitted transactions from existing WAL history. |
| **2. During Dirty-Page Flush** | `DuringCheckpointDirtyPageFlush` | Some dirty pages flushed, others unwritten; checkpoint record not written; header unchanged. | Redo pass re-applies deltas for unwritten pages; undo pass rolls back losers. |
| **3. During Checkpoint WAL Flush** | `DuringCheckpointWalFlush` | Dirty pages flushed to disk; checkpoint record written to buffer but not durable; header unchanged. | Header still references prior checkpoint; recovery re-evaluates WAL from prior checkpoint. |
| **4. During Header Persistence** | `DuringCheckpointHeaderPersistence` | Dirty pages flushed; checkpoint record durable in WAL; Page 0 header not yet updated. | Page 0 references prior checkpoint; recovery begins from prior checkpoint and replays cleanly. |
| **5. Before WAL Truncation** | `BeforeCheckpointWalTruncation` | Checkpoint record durable; Page 0 header updated with new checkpoint LSN; WAL untruncated. | Recovery begins from new checkpoint; all records prior to checkpoint are present but ignored. |
| **6. During WAL Truncation** | `DuringCheckpointWalTruncation` | Checkpoint complete; prefix truncation interrupted in scratch/backup file. | Recovery opens intact WAL; recovers without corruption or missing history. |
| **7. After Completed Checkpoint** | `AfterCheckpointCompleted` | Checkpoint complete; Page 0 header updated; WAL truncated to retention boundary. | Recovery starts from new checkpoint in truncated WAL; state is immediately consistent. |
| **8. Active Transaction Spanning Checkpoint** | N/A (Interleaved) | Transaction started before checkpoint, continues after checkpoint, crashes without commit. | Checkpoint records active transaction ID; recovery traces and undoes all operations. |

---

## 4. Phase 73 Verifier as a Diagnostic Oracle

Phase 74 integrates the Phase 73 cross-layer consistency verification engine (`TreeDb::check_file`, `TreeDb::verify_file`, `TreeDb::check_consistency`, `TreeDb::verify_consistency`):

### 4.1 Pre-Recovery File Inspection
- `TreeDb::check_file` opens the raw database file and WAL without executing `RecoveryEngine::recover` (`TreeDb::open_read_only_unrecovered`).
- **Unapplied WAL Tail Detection**: When a crash occurs with mutations beyond `last_checkpoint_lsn`, the verifier identifies unapplied log records and emits `ConsistencyIssueKind::UnappliedWalTail` with `ConsistencySeverity::Info`.
- This explicitly documents that crash recovery is a prerequisite for offline consistency rather than silently modifying disk state.

### 4.2 Post-Recovery Certification
- Following `TreeDb::open`, the verifier audits the recovered database across all 7 check passes:
  1. `"database_header"`: File size, header magic, format version, root page bounds.
  2. `"freelist_invariants"`: Freelist count agreement, cycle freedom, reserved page protection.
  3. `"catalog_metadata"`: Catalog page decodability, table and index definitions.
  4. `"page_ownership"`: Disjoint ownership mapping, no duplicate ownership between tables and freelist.
  5. `"primary_trees_and_record_stores"`: Primary index head alignment with valid record slots, version chain ordering.
  6. `"secondary_indexes"`: Secondary key matches column values, covering payload equality, unique constraint enforcement.
  7. `"wal_state"`: WAL header magic, CRC32 record integrity, LSN monotonicity.
- Post-recovery assertions verify `report.is_healthy() == true`, `report.truncated == false`, and `report.has_fatal() == false`.

---

## 5. Logical-State Comparison Methodology

To prevent circular validation where the verifier checks storage structures against themselves, Phase 74 uses an **independent differential reference model** (`LogicalModel`):
- Maintained as an in-memory `BTreeMap<i64, ExpectedAccount>` tracking only committed operations.
- **Table Scan Verification**: Scans physical table rows and asserts exact 1:1 match with committed rows.
- **Primary Key Lookups**: Fetches every expected row via primary index and asserts column values match.
- **Secondary Index Lookups**: Exercises index paths via SQL queries and validates covering/composite projections.
- **SQL Aggregates**: Executes `SELECT count(*) FROM accounts` to verify end-to-end query planner consistency.

---

## 6. Test Suites & Coverage Summary

The dedicated test suite in `tests/recovery_certification.rs` comprises 20 comprehensive integration tests:

| Test Case | Objective |
|---|---|
| `test_recovery_uncommitted_insert_rollback_and_unique_key_reuse` | Uncommitted insert rolled back; unique key released and reusable. |
| `test_recovery_uncommitted_update_and_covering_payload_retention` | Uncommitted update rolled back; covering index payload retains committed value. |
| `test_recovery_uncommitted_delete_retains_row` | Uncommitted delete rolled back; row remains visible. |
| `test_recovery_uncommitted_overflow_record_does_not_corrupt_storage` | Large uncommitted multi-page overflow allocation rolled back safely. |
| `test_recovery_durable_commit_survives_without_checkpoint` | Committed transactions survive crash even if dirty pages were never checkpointed. |
| `test_recovery_commit_seq_monotonicity_across_restarts` | `CommitSeq` advances strictly monotonically across restart cycles without collisions. |
| `test_recovery_historical_mvcc_snapshot_visibility` | Multi-version chain (v1 -> v2 -> v3) readable by historical snapshots after recovery. |
| `test_recovery_after_vacuum_reclamation` | Vacuum page reclaims and version pruning survive restart and recovery. |
| `test_checkpoint_boundary_1_before_checkpoint_initiation` | Crash before checkpoint initiation recovers cleanly. |
| `test_checkpoint_boundary_2_during_dirty_page_flush` | Crash during checkpoint dirty page flushing recovers cleanly. |
| `test_checkpoint_boundary_3_during_wal_flush` | Crash during checkpoint WAL flush recovers cleanly. |
| `test_checkpoint_boundary_4_during_header_persistence` | Crash during Page 0 header persistence recovers cleanly. |
| `test_checkpoint_boundary_5_before_wal_truncation` | Crash before WAL truncation recovers cleanly. |
| `test_checkpoint_boundary_6_during_wal_truncation` | Crash during WAL truncation recovers cleanly. |
| `test_checkpoint_boundary_7_after_checkpoint_completed` | Crash after completed checkpoint recovers cleanly. |
| `test_checkpoint_boundary_8_active_transaction_spanning_checkpoint` | Active uncommitted transaction spanning checkpoint is properly rolled back. |
| `test_recovery_idempotency_repeated_cycles` | 5 consecutive reopen and recovery cycles produce identical logical state. |
| `test_recovery_followed_by_checkpoint_and_reopen_cycles` | Interleaved recovery, checkpoint, reopen, and mutation cycles maintain integrity. |
| `test_verifier_diagnostics_pre_recovery_unapplied_wal_tail_vs_post_recovery_clean` | Verifier detects unapplied WAL tail pre-recovery; certifies clean post-recovery. |
| `test_metamorphic_differential_recovery_fixed_seeds` | Metamorphic differential testing across 4 fixed seeds (42, 1337, 2026, 9999). |

---

## 7. How to Reproduce Focused Tests

Run the dedicated recovery certification suite:
```bash
cargo test --test recovery_certification
```

Run related transaction atomicity, consistency, and recovery suites:
```bash
cargo test --test transaction_atomicity
cargo test --test storage_consistency
cargo test --test mvcc_recovery
cargo test --test mvcc_conflicts
cargo test --test mvcc_reference_model
cargo test --test mvcc_vacuum
cargo test --test mvcc_indexes
```

Run formatting and Clippy audits:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Run full repository test suite:
```bash
cargo test --all
```

---

## 8. Known Unsupported Boundaries & Remaining Limitations

1. **Single Writer Execution**:
   - Single-threaded writer transaction model is maintained. Concurrent writer execution, distributed transactions, and SSI are explicitly out of scope.
2. **No Automatic Database Repair**:
   - Recovery restores transactions from WAL; it does not attempt heuristic repair of corrupted files.
3. **Orphaned Page Classification**:
   - When a transaction allocates overflow pages but crashes before commit, the pages are unlinked from active tables and classified as orphan warnings (`ConsistencySeverity::Warning`) until reclaimed by vacuum. This is an expected physical allocation state and does not constitute corruption.
