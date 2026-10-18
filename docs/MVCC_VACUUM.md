# TreeDB MVCC Vacuum, Version Garbage Collection & Storage Reclamation

## Overview

Phase 70 introduces a safe, deterministic, transactional Multi-Version Concurrency Control (MVCC) vacuum and storage reclamation engine for TreeDB. Prior to Phase 70, historical record versions, superseded index entries, and multi-page overflow chains were retained indefinitely in the slotted record pages and B+ Tree structures.

TreeDB's vacuum engine enforces the foundational MVCC safety invariant:

> **A physical version may be reclaimed only when it is provably unnecessary to every active snapshot and every future snapshot permitted by the engine's isolation contract.**

The reclamation protocol coordinates across the active snapshot registry, primary B+ Tree index, secondary and covering indexes, slotted record pages, multi-page overflow chains, the free-page list, and Write-Ahead Logging (ARIES recovery).

---

## 1. Reclamation Horizon

The reclamation horizon represents the boundary of historical visibility required by active transactions and snapshots:

```rust
pub struct ReclamationHorizon {
    pub oldest_required_commit_seq: CommitSeq,
}
```

The horizon is derived from the database-level `SnapshotRegistry`:
1. **With Active Snapshots**:
   $$\text{Horizon} = \min_{s \in \text{active\_snapshots}} \{ s.\text{as\_of\_commit\_seq} \}$$
   No version required by any active snapshot at or after its visibility boundary can be reclaimed.
2. **With No Active Snapshots**:
   Under TreeDB's single-node, monotonic isolation contract, future transactions and snapshots always observe a commit sequence horizon $S \ge \text{current\_commit\_seq}$. Therefore, when no snapshots are active in the registry, the horizon advances to `current_commit_seq`, making all versions superseded prior to or at `current_commit_seq` eligible for pruning.

---

## 2. Active-Snapshot Registry Semantics

The `SnapshotRegistry` tracks all point-in-time snapshots active in the system:
* Snapshots are registered when a transaction begins (`db.begin()`) or when a standalone point-in-time read handle is requested (`db.register_snapshot()`).
* Snapshots are identified by unique monotonic `SnapshotId` values and mapped to their immutable `as_of_commit_seq`.
* `SnapshotRegistry::reclamation_horizon` runs in $O(N)$ or $O(1)$ time over active snapshot commit sequences.
* Standalone and transaction snapshots are cleanly released upon commit, explicit abort, or automatic rollback during `Drop`.
* Consistency checks (`SnapshotRegistry::validate_consistency`) detect orphaned, corrupted, or future-dated snapshot horizons.

---

## 3. Version Eligibility Rules

Version eligibility is evaluated centrally via `evaluate_version_eligibility`:

```rust
pub enum VersionReclamationEligibility {
    LiveHead,
    RequiredBySnapshot { as_of: CommitSeq },
    CreatedAfterHorizon { created_at: CommitSeq, horizon: CommitSeq },
    ActiveTransaction { xmin: TxnId },
    EligibleAborted { xmin: TxnId },
    EligibleDeletedRow { deleted_at: CommitSeq },
    EligibleSuperseded { deleted_at: CommitSeq },
}
```

A version is safe to reclaim if and only if:
1. It is **NOT** the live head of an undeleted logical record (`LiveHead`).
2. Its creating transaction `xmin` is committed, or if aborted (`EligibleAborted`), no active transaction or rollback references it.
3. No active snapshot can see it:
   For every active snapshot $S$:
   * If created after $S$ ($c_{\text{seq}} > S.\text{as\_of}$), it is invisible to $S$, but may be required by a newer snapshot.
   * If created at or before $S$ ($c_{\text{seq}} \le S.\text{as\_of}$):
     * If deleted at $d_{\text{seq}} > S.\text{as\_of}$, it is **REQUIRED** by $S$ and must NOT be reclaimed.
     * Only if $d_{\text{seq}} \le \text{Horizon}$ is it provably superseded for ALL active and future snapshots.
4. Its deletion is fully committed.

---

## 4. Deleted Logical Record Cleanup

When an entire row has been logically deleted (`DELETE FROM table WHERE ...`):
1. The logical record's head version is marked with `deleted_at = Some(commit_seq)`.
2. When `deleted_at <= Horizon`:
   * No active or future snapshot can observe any version of this logical record.
   * Vacuum removes the primary key entry from the primary B+ Tree.
   * All secondary and covering index entries referencing any version in the chain are deleted.
   * Multi-page overflow chains belonging to each version are freed back to the PageManager.
   * The slotted record entries are deleted, freeing space within the record page.
   * Primary keys become immediately available for reuse in subsequent inserts without colliding with obsolete history.

---

## 5. Aborted-Version Cleanup

Versions created by aborted transactions (`xmin` aborted or rollback CLR logged) never become visible to committed snapshots:
* Vacuum verifies that the creating transaction is not active in the buffer pool.
* If an aborted version is encountered at the head or within a version chain, vacuum removes secondary index entries pointing to it, frees its overflow pages, and prunes the version from storage.

---

## 6. Secondary and Covering Index Cleanup

TreeDB supports single-column, composite, unique, and covering secondary indexes:
* Secondary index entries store `(IndexKey, PrimaryKey, RecordId, Payload)`.
* In MVCC, intermediate versions generated by `UPDATE` create physical records with corresponding secondary index entries.
* During version reclamation:
  * For each reclaimed physical `RecordId`, vacuum extracts the row's values and deletes the associated secondary/covering index entry.
  * Unique indexes are kept strictly consistent; removing obsolete entries prevents historical versions from causing false unique constraint violations upon subsequent insertions.
  * Covering index entries containing stored payloads are removed atomically with the parent version, ensuring index-only scans never observe stale or freed payload data.

---

## 7. Overflow-Page Reclamation

Records exceeding the inline record size threshold ($> 1024$ bytes) are stored across linked `OverflowPage` chains:
* A slotted record contains an 8-byte `OverflowDescriptor` pointing to the first overflow page.
* When a version is reclaimed:
  * `RecordStore::delete_and_count` traverses the `next_page_id` chain of overflow pages.
  * Each overflow page is recycled into the PageManager's free-page list via `buffer_pool.free_page(pid)`.
  * The operation verifies page ownership to guarantee no retained version references the freed overflow pages.
  * Reclaimed overflow pages are tracked in `VacuumResult.overflow_pages_reclaimed`.

---

## 8. Record-Page Slots and Compaction

Slotted record pages maintain an array of 4-byte slot pointers at the page header:
* Reclaimed slots are marked as deleted (offset set to 0).
* When a record page accumulates deleted slots or fragmented space, internal page compaction coalesces surviving records toward the end of the page, maintaining slot stability.
* Completely empty record pages (where all slots are deleted and free space equals `PAGE_BODY_SIZE`) are detached and recycled directly into the freelist via `RecordStore::try_free_empty_record_page`.

---

## 9. Freelist Integration

Freed record pages and overflow pages are integrated into TreeDB's persistent free-page list:
* `BufferPool::free_page` pushes freed pages to the head of the database header's freelist (`freelist_head`, `free_page_count`).
* Invariants maintained:
  * No page containing retained versions is ever freed.
  * No page is inserted into the freelist twice (cycle-free and duplicate-free).
  * No page is simultaneously marked allocated and free.
  * Page allocations reuse freelist pages before growing the database file.

---

## 10. WAL Ordering and Recovery

Vacuum operations are fully logged through TreeDB's ARIES-style WAL stream:
* Physical modifications to B+ Tree pages, record pages, and freelist headers generate page-delta log records (`PageDelta`).
* Vacuum commits within a top-level transactional context or auto-committed transaction, flushing WAL through the commit LSN.
* If a crash occurs during vacuum:
  * ARIES Redo reapplies all logged page modifications up to the crash point.
  * ARIES Undo rolls back incomplete transactions using CLRs.
  * Post-recovery consistency validation verifies all version chains, index trees, and freelist structures.

---

## 11. Vacuum API and Resource Limits

Vacuum is invoked programmatically with bounded resource quotas:

```rust
pub struct VacuumConfig {
    pub max_versions: usize,
    pub max_pages: usize,
}

pub struct VacuumResult {
    pub versions_examined: u64,
    pub versions_reclaimed: u64,
    pub pages_examined: u64,
    pub pages_reclaimed: u64,
    pub overflow_pages_reclaimed: u64,
    pub index_entries_removed: u64,
    pub bytes_reclaimed: u64,
    pub active_snapshots: usize,
    pub reclamation_horizon: CommitSeq,
    pub blocked_by_oldest_snapshot: bool,
}
```

The programmatic entry points include:
* `db.vacuum(&config)`: Full-database vacuum across all tables and default KV store.
* `db.vacuum_table("t1", &config)`: Table-specific vacuum.
* `txn.vacuum(&config)`: Transaction-scoped vacuum honoring transaction snapshot horizons.
* `db.validate_vacuum_consistency()`: Post-vacuum structural validation.

---

## 12. Idempotency

Running vacuum repeatedly without intervening database mutations is completely idempotent:
* The first pass reclaims all eligible versions and updates version chains.
* Subsequent passes examine surviving records, find 0 eligible versions, and make zero storage mutations.
* Freelist counts and tree roots remain unchanged across repeated runs.

---

## 13. Known Limitations

1. **Single-threaded Execution**: Vacuum executes synchronously on the calling thread without background worker threads.
2. **Table-at-a-time Granularity**: Incremental passes scan tables sequentially up to the configured `max_versions` and `max_pages` bounds.
3. **Primary Key Updates**: Primary key mutations in TreeDB require delete-and-insert rather than in-place version chain updates.

---

## 14. Future Concurrency Implications

When concurrent transactions and background vacuum threads are introduced in future phases:
* Row-level or page-level read-write latches will be required to coordinate vacuum traversal with concurrent table updates.
* Snapshot registration will require lock-free or read-copy-update (RCU) active snapshot tracking to avoid contention during horizon calculations.
* Version-chain link rewriting must be made atomic using latch coupling or optimistic version validation.

---

## 15. Phase 71: Interaction with Transaction Conflict Detection

Vacuum operations interact cleanly with Phase 71 commit validation:
* **Horizon Protection**: Vacuum never reclaims versions within the active snapshot horizon, ensuring transactions can always read their targeted historical versions.
* **Unique Index Hygiene**: By purging obsolete historical index entries, vacuum prevents false unique index collisions on key re-insertions while maintaining correct commit-validation checks for live versions.
* **Validation Invariance**: Vacuum runs do not mutate live committed head versions; consequently, an in-progress transaction validating against the committed head state remains consistent across vacuum runs.
* See [MVCC_CONFLICTS.md](MVCC_CONFLICTS.md) for complete details.

---

## 16. Phase 72: Vacuum Failure Injection & Crash Interruption Invariants

Phase 72 verifies that vacuum operations behave atomically under simulated failure injection and unexpected crashes:
* **Interrupted Index Cleanup**: If vacuum fails during secondary index pruning (`DuringVacuumIndexCleanup`), un-pruned entries are safely cleaned up on subsequent vacuum passes without corrupting live lookups.
* **Interrupted Record Reclamation**: If vacuum is interrupted while reclaiming record slots or overflow chains (`DuringVacuumRecordReclaim`), physical pages and version chain cuts remain structurally valid.
* **Interrupted Freelist Update**: If vacuum encounters a crash while returning empty record pages to the freelist (`DuringVacuumFreelistUpdate`), recovery safely reconciles disk state without freelist cycles or duplicate pages.
* See [TRANSACTION_ATOMICITY.md](TRANSACTION_ATOMICITY.md) for full failure injection details.

---

## 17. Phase 73: Cross-Layer Consistency Verification Integration

Phase 73 introduces non-destructive, read-only cross-layer consistency verification (`TreeDb::check_consistency`, `TreeDb::verify_file`) that thoroughly checks post-vacuum state:
* Confirms that vacuum-reclaimed versions are no longer referenced by live primary or secondary index entries.
* Confirms that reclaimed record and overflow pages added to the freelist do not appear in any active table ownership graph.
* Guarantees that running consistency verification after vacuum never modifies the database file or triggers unintended vacuum side effects.
* See [STORAGE_CONSISTENCY.md](STORAGE_CONSISTENCY.md) for complete details.
