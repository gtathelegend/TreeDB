# Transaction Atomicity, Crash Recovery Invariants & Failure Injection Audit

TreeDB — Persistent Storage Engine

## 1. Overview and Atomicity Contract

Phase 72 formalizes and verifies the atomicity, crash recovery, and durability invariants of TreeDB's transactional storage engine across operation failures, transaction aborts, deterministic failure injection, and process restarts.

TreeDB enforces the following externally observable contracts:

1. **Successful Commit**:
   - All mutations (record versions, primary B+ Tree updates, secondary/covering index entries, overflow chains) become durable.
   - A sequential logical `CommitSeq` is assigned and recorded in both the version metadata and the durable WAL commit record.
   - All changes are atomic and visible to future snapshots with `snapshot.as_of_commit_seq >= commit_seq`.
2. **Explicit Transaction Abort**:
   - A transaction calling `abort()` immediately transitions to `TxnState::Aborted`.
   - All modifications recorded in the transaction's write set are physically undone or marked obsolete.
   - Any uncommitted physical record versions or index entries are discarded or invalidated; no partial changes are visible to any snapshot.
3. **Commit Validation Failure**:
   - If commit-time conflict detection (e.g. write-write conflict, stale version write, unique index violation) fails, the transaction immediately transitions to `TxnState::Aborted`.
   - The WAL never receives a `Commit` record.
   - All partial in-memory mutations are aborted and reverted.
4. **Execution Error After Modifications**:
   - Encountering an error (e.g. duplicate key, schema violation, injected fault) during transaction execution halts execution.
   - Dropping the transaction or calling `abort()` rolls back all intermediate modifications.
   - No partially executed transaction changes leak into subsequent reads.
5. **Crash Prior to Durable Commit Record**:
   - If the process is interrupted before the WAL commit record is durably flushed to disk, the transaction is treated as a loser transaction during ARIES crash recovery.
   - The undo pass rolls back all modified pages to their before-images, emitting Compensation Log Records (CLRs).
   - Upon completion of recovery, the transaction's changes are completely absent.
6. **Crash After Durable Commit Record**:
   - Once the WAL commit record has been flushed to disk (`AfterDurableCommitRecordFlush`), the commit is irrevocable.
   - Even if subsequent in-memory operations, cache invalidations, or checkpoint header updates are interrupted by a crash, ARIES redo replays all logged changes.
   - Upon recovery, the transaction remains fully committed, visible, and indexed.
7. **Reopening After Incomplete Transaction**:
   - Recovery executes analysis, redo, and undo passes.
   - Incomplete (uncommitted) transactions are rolled back; committed transactions are redone.
   - Storage allocators, freelists, indexes, and version chains return to a clean, consistent state.
8. **Repeated Recovery Idempotency**:
   - Reopening the database repeatedly without intervening writes produces identical logical rows, index contents, and freelist structures.
   - Subsequent `CommitSeq` allocations do not regress or collide with previous commits.

---

## 2. The Commit Protocol & Durable Commit Point

The commit lifecycle in `src/engine/transaction.rs` follows an exact, verified ordering:

1. **Transaction State Validation**:
   - Check that `state == TxnState::Active`. Rejects transitions from `Created`, `Committed`, or `Aborted`.
2. **Commit Validation Stage**:
   - Validate read sets and write sets against intervening commits (`validate_commit()`).
   - Validate secondary unique-index constraints against concurrent committed entries.
   - If validation fails, `self.abort_internal()` is invoked, rolling back mutations and returning an error.
3. **Commit-Sequence Allocation**:
   - Allocate a strictly monotonic logical `CommitSeq = db.allocate_commit_seq()`.
4. **MVCC Version Metadata Stamping**:
   - Stamp `created_at = Some(commit_seq)` on all newly inserted record versions in `write_set.inserted_records`.
   - Stamp `deleted_at = Some(commit_seq)` on all updated and deleted predecessor records.
   - *Ordering Invariant*: Metadata is stamped while `active_txn` is still registered on the `BufferPool`, ensuring all metadata page mutations generate WAL `PageDelta` records.
5. **Durable Commit Point (WAL Commit Record & Flush)**:
   - Call `wal.commit_txn_with_seq(txn_id, commit_seq.0)`.
   - Append `LogRecordType::Commit` with `commit_seq` to the WAL.
   - **Flush the WAL**: Perform an immediate physical `flush()` through the commit LSN to storage.
   - *Durable Boundary*: This WAL flush is the exact irrevocable commit point.
6. **State Transition and Cache Invalidation**:
   - Transition `self.state = TxnState::Committed`.
   - Release owned snapshot in `SnapshotRegistry`.
   - Invalidate catalog generation and query plan cache if catalog mutations occurred.

---

## 3. Deterministic Failure Injection Framework

The test-only failure injection subsystem is defined in `src/engine/failure.rs`:
- Controlled via `StorageFailurePoint` enum.
- Injected via `BufferPool::set_test_failure_point(Option<StorageFailurePoint>)`.
- Disabled by default (`None`), introducing zero overhead or behavior alteration in production.
- Disarms automatically when triggered, ensuring deterministic, one-shot failure semantics.

### Tested Failure Boundaries (16 Named Points):

| Failure Point | Location | Recovery Expectation |
|---|---|---|
| `BeforeFirstMutation` | `Table::insert`, `update`, `delete` | Transaction never starts mutation; clean rollback. |
| `AfterRecordVersionAllocated` | `RecordStore::insert_version` | Slot allocated but uncommitted; rolled back by ARIES undo. |
| `AfterRecordPayloadLogged` | `BufferPool::unpin_page` | Data page dirtied and logged; uncommitted; rolled back by ARIES undo. |
| `AfterVersionMetadataChanged` | `RecordStore::update_version_metadata` | Version header modified; rolled back by ARIES undo. |
| `AfterPrimaryIndexModified` | `Table::insert`, `update`, `delete` | Primary B+ Tree modified; rolled back by ARIES undo. |
| `AfterSecondaryIndexModified` | `Table::insert`, `update`, `delete` | Secondary index modified; rolled back by ARIES undo. |
| `DuringOverflowPageAlloc` | `RecordStore::allocate_overflow_chain` | Multi-page overflow chain interrupted; rolled back, no orphan pages. |
| `DuringRecordSlotReclaim` | `RecordStore::delete_and_count` | Slot reclamation aborted; row restored to pre-delete state. |
| `DuringFreelistUpdate` | `BufferPool::free_page` | Freelist unpin/link fault; rolled back cleanly. |
| `BeforeCommitRecordLogged` | `LogManager::commit_txn_with_seq_and_fault` | Logged mutations exist but no commit record; treated as loser txn. |
| `AfterCommitRecordLoggedBeforeFlush` | `LogManager::commit_txn_with_seq_and_fault` | Truncates unflushed commit tail; recovery rolls back transaction. |
| `AfterDurableCommitRecordFlush` | `LogManager::commit_txn_with_seq_and_fault` | Commit record durable; recovery redoes transaction, row committed! |
| `DuringHeaderPersistence` | `PageManager::write_header_to_disk` | Checkpoint header write fails; recovery reconstructs safe state. |
| `DuringVacuumIndexCleanup` | `VacuumEngine::reclaim_version_and_indexes` | Vacuum interrupted during index pruning; safe for restart. |
| `DuringVacuumRecordReclaim` | `VacuumEngine::reclaim_version_and_indexes` | Vacuum interrupted during slot/overflow reclaim; safe for restart. |
| `DuringVacuumFreelistUpdate` | `VacuumEngine::vacuum_table` | Vacuum interrupted during empty page freeing; freelist reconciled. |

---

## 4. Crash Recovery Invariants & Idempotency

### ARIES Redo Pass:
- Replays history forward from the minimum `rec_lsn` across the Dirty Page Table (DPT).
- Replays all `PageDelta` and `Clr` records where `rec.lsn > page.lsn()`.
- Brings physical pages exactly to the state at the moment of the crash.

### ARIES Undo Pass:
- Identifies all active uncommitted transactions ("loser transactions").
- Undoes their logged mutations in reverse chronological order using `prev_lsn` chains.
- Emits Compensation Log Records (CLRs) to ensure undo actions are never undone twice.
- Appends `Abort` records when transactions are fully rolled back.

### Recovery Idempotency & CommitSeq Monotonicity:
- Running recovery multiple times in succession without intervening mutations is strictly idempotent.
- Recovery parses the durable `max_commit_seq` from both the database header and WAL records, setting `next_commit_seq = max(header, wal).saturating_add(1)`.
- Subsequent transactions allocate strictly higher sequence numbers without collisions or sequence regression.

---

## 5. Index & Allocator Structural Invariants

After any crash recovery scenario, the following structural invariants are verified:

1. **Primary Index (`BPlusTree`)**:
   - All live rows resolve to the current head version.
   - Aborted insertions leave zero primary index entries.
   - Deleted keys are completely absent from scans and lookups.
   - Reopening preserves all valid primary key entries.
2. **Secondary & Covering Indexes (`SecondaryIndex`)**:
   - Every secondary index entry resolves to a valid record version.
   - Covering index payloads match table columns exactly.
   - Aborted updates/deletes do not leave dangling index entries.
   - Unique constraints remain valid and reject duplicates.
3. **Record Store & Overflow Chains (`RecordStore`)**:
   - Inline records and multi-page overflow chains decode without corruption.
   - No overflow chain loops, out-of-bounds page IDs, or reserved page references.
   - Reclaimed slots are never exposed as live data.
4. **Freelist & Page Allocation (`FreePageManager`)**:
   - Zero duplicate free-page entries.
   - Zero freelist cycles.
   - No page is simultaneously free and referenced by an index or table.
   - Reclaimed overflow and record pages can be safely reallocated.

---

## 6. Consistency Verification & Recovery Certification Integration

* **Phase 73 Consistency Verification**: Programmatic read-only consistency verification (`TreeDb::check_consistency`, `TreeDb::verify_file`) systematically validates cross-layer structural and logical invariants with 100% byte-for-byte file preservation.
* **Phase 74 Recovery Certification**: Comprehensive checkpoint crash matrix validates all 8 persistence boundaries (before checkpoint, during dirty-page flush, during WAL flush, during header sync, before/during WAL truncation, after completion, active transaction spanning checkpoint) and certifies recovery idempotency. See [RECOVERY_CERTIFICATION.md](RECOVERY_CERTIFICATION.md).

---

## 7. Known Unsupported Boundaries & Remaining Limitations

1. **Single Writer Execution**:
   - TreeDB supports concurrent readers with MVCC snapshots, but writer transactions must execute sequentially. Concurrent writer execution, multithreading, and distributed transactions remain explicitly disabled.
2. **Unflushed Operating System Caches (Simulated vs Real Crash)**:
   - File truncate and fault points simulate unflushed WAL tails and process termination. TreeDB relies on OS/filesystem `fsync()` compliance for durability guarantees against hardware power loss.
3. **No Automatic Client Retries**:
   - Transactions aborted due to conflicts or errors must be retried by the application on a fresh snapshot.
