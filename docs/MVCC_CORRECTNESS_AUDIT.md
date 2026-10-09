# MVCC Correctness Audit — Phase 69

## Executive Summary

Phase 69 hardens TreeDB's Multi-Version Concurrency Control (MVCC) engine introduced in Phase 68. The primary objective of this phase is to establish mathematical correctness, durability, and physical consistency across historical version payloads, index access paths, multi-page overflow chains, crash-recovery boundaries, and prepared statement plan caching, prior to introducing concurrency or multithreading.

All expectations are verified against an independent, in-memory reference model and tested with deterministic crash injection and differential randomized testing.

---

## 1. Logical-Record vs. Physical-Version Identity

### Architectural Model
- **Logical Record Identity**: Represents the table row as an entity identified by its Primary Key (or logical row identity). In the primary B+ Tree, the primary key always points to the newest physical record version (`head_rid`).
- **Physical Version Identity (`VersionId`)**: Strongly-typed newtype `VersionId(pub RecordId)` representing a specific physical version occupying a designated slot `(page_id, slot_id)` in the record store.
- **Index Pointing Discipline**:
  - The Primary Key index B+ Tree points to the newest head version `RecordId`.
  - Secondary indexes point to specific physical versions (`VersionId`). When an indexed row is updated, the previous index entry is **not** deleted; instead, a new index entry is inserted pointing to the new version `VersionId`.
  - Covering index payloads are bundled with the specific version entry to guarantee that index-only scans never return fields from a newer or older version.

---

## 2. Version-Chain Representation

### On-Disk Structure
Every record in `PageType::Record` (or overflow head page) contains a persistent 46-byte version header:
- `format_version`: Version metadata format identifier (currently 1).
- `xmin`: `TxnId` of the creator transaction.
- `xmax`: Optional `TxnId` of the deleting/superseding transaction.
- `created_at`: Optional `CommitSeq` stamped when creator transaction commits.
- `deleted_at`: Optional `CommitSeq` stamped when deleting transaction commits.
- `prev_version`: Optional `RecordId(prev_page_id, prev_slot_id)` pointing backward to the immediate predecessor version.

### Traversal and Guardrails
- Chains are traversed newest-to-oldest via `RecordStore::read_visible`.
- **Immediate self-cycle prevention**: `prev_version == current_rid` is intercepted immediately with `Error::VersionChainCycle`.
- **General cycle detection**: Traversal maintains a `HashSet<RecordId>`. Re-visiting any `RecordId` terminates with `Error::VersionChainCycle`.
- **Maximum depth limit**: A hard limit `MAX_VERSION_CHAIN_DEPTH = 1000` bounds chain traversal stack and I/O consumption, returning `Error::VersionChainTooDeep`.

---

## 3. Historical-Payload Preservation

### The Invariant
> An update or delete must never overwrite or mutate the payload of any existing historical version. Historical versions must remain fully readable by any active or future snapshot whose visibility horizon encompasses them.

### Implementation
- Updates allocate a new slot for the new version payload and header. The predecessor version's header has only its `xmax` and `deleted_at` fields updated in-place via `RecordStore::update_version_metadata`.
- Deletions only update `xmax` and `deleted_at` in the existing version's header in-place. The payload bytes are untouched.
- Tested across:
  - Small inline records.
  - Near-boundary records (4000+ bytes).
  - Multi-page overflow records (10KB+).
  - Multi-column schemas and NULL attributes.
  - Deep update chains (50+ updates on a single row).

---

## 4. CommitSeq Durability and Recovery

### Monotonic Ordering and Durability Flow
```text
Version metadata and payload changes written
                    |
                    v
          WAL record appended & flushed
                    |
                    v
        Commit WAL record written & flushed
                    |
                    v
         Durable CommitSeq allocated
                    |
                    v
        DbHeader::max_commit_seq durable
                    |
                    v
            Snapshot visibility
```

### Invariants Verified
1. **Monotonicity**: `CommitSeq` is strictly monotonic across restarts and crashes. Reopening the database recovers `next_commit_seq = max_commit_seq + 1`.
2. **Aborted Invisibility**: Uncommitted or aborted writes never receive a `CommitSeq` and are treated as invisible by all other transactions.
3. **Crash Survivability**: Transactions whose commit record was durable in the WAL survive crash recovery with intact version chains, index links, and overflow chains.

---

## 5. Index and Covering-Index Semantics

### Root Cause of Phase 68 Defect
In Phase 68, `Table::update` and `Table::delete` physically deleted old `RecordId`s from secondary indexes (`sec_idx.delete`). When an indexed column was updated or deleted, older historical snapshots querying through the secondary index observed 0 rows. Furthermore, covering index scans evaluated version-chain visibility rather than exact version visibility, leaking newer covering payloads to older snapshots.

### Phase 69 Hardening
1. **Secondary Index Retention**: `Table::update` and `Table::delete` retain historical index entries. Secondary indexes store all historical `(indexed_key, VersionId, payload)` mappings.
2. **Exact Version Visibility**: Secondary index access paths (`index_lookup`, `index_scan`, `composite_index_lookup`, `composite_index_scan`, `index_skip_scan`) evaluate `RecordStore::is_version_visible` and `read_exact_version_if_visible`. They verify whether the exact physical version pointed to by the index entry is visible to the transaction snapshot, completely eliminating duplicate rows, stale reads, and chain traversal confusion.
3. **Covering Index Consistency**: In `index_only_scan` and `index_only_skip_scan`, entries are filtered using `is_version_visible(entry.rid)`. Because the covering payload stored in the index entry belongs to that exact physical version, no payload data from newer or older versions can leak.
4. **Unique Constraints**: Unique constraint checks during `insert` and `update` evaluate `is_version_visible(existing_rid)`, allowing primary and secondary unique key reuse after committed deletions while rejecting concurrent or active collisions.

---

## 6. Overflow Storage Preservation

- Multi-page overflow records store `VersionMetadata` on the overflow head page (`head_page_id`), chaining to subsequent overflow pages.
- When an overflow record is updated or deleted, `RecordStore::update_version_metadata` modifies only the 46 header bytes on the overflow head page.
- Because `RecordStore::delete()` is never invoked for MVCC updates/deletes, no overflow page belonging to a historical version is ever returned to the freelist or reused by another record.
- Historical snapshots can read multi-page overflow versions across arbitrarily deep update chains.

---

## 7. Page Compaction and Reuse Safety

- **Slotted-Page Compaction (`RecordPage::compact`)**: Slotted-page compaction reclaims fragmented space within a single page by sliding active payload bytes toward the end of the page. It **never** alters slot indices, reassigns slot IDs, or re-indexes slot headers.
- Since historical versions remain active slots (marked with valid offsets, not tombstoned), compaction never moves a version to a different `(page_id, slot_id)` address.
- Therefore, all `prev_version` links and secondary index `RecordId` references remain 100% stable across page compaction cycles.

---

## 8. Failure-Injection Matrix Results

Deterministic failure injection in `tests/mvcc_crash_matrix.rs` tested 9 critical crash boundaries:
1. **Crash Boundary 1 (Pre-mutation)**: Crash before persistent mutation leaves database cleanly unchanged.
2. **Crash Boundary 2 (Unflushed WAL tail)**: Crash with un-durable WAL tail discards uncommitted changes; table remains at initial state.
3. **Crash Boundary 3 (Data page modified uncommitted)**: Data page modified in buffer pool before crash; WAL recovery correctly aborts uncommitted transactions.
4. **Crash Boundary 4 (Version-chain metadata abort)**: Uncommitted version inserted into chain; after crash recovery, readers see only the previous committed version.
5. **Crash Boundary 5 (Index maintenance uncommitted)**: Secondary index entry inserted by uncommitted transaction; after crash recovery, index lookup returns only committed records.
6. **Crash Boundary 6 (Commit record durable)**: Commit record durable in WAL; crash recovery recovers full version chain and committed payload.
7. **Crash Boundary 7 (Failure during recovery)**: Crash during recovery replay; subsequent recovery restart safely completes replay idempotently.
8. **Crash Boundary 8 (Post-recovery pre-checkpoint)**: System crashes after recovery before checkpoint; subsequent reopen recovers state without data loss or corruption.
9. **Crash Boundary 9 (Overflow multi-page crash recovery)**: Multi-page overflow updates crash before and after commit; overflow chains survive intact.

All 9 crash injection scenarios pass deterministically.

---

## 9. Reference-Model Methodology & Differential Testing

### Independence of Reference Model
`tests/mvcc_reference_model.rs` constructs an independent in-memory MVCC model (`RefEngine`, `RefTable`, `RefLogicalRecord`, `RefVersion`, `RefSnapshot`).
- The reference model **does not** call or depend on `MvccVisibilityPolicy`.
- It implements pure functional visibility logic:
  - `xmin` visibility: visible if created by self or committed with `CommitSeq <= snapshot.read_commit_seq`.
  - `xmax` visibility: deleted if deleted by self or committed deletion with `CommitSeq <= snapshot.read_commit_seq`.
- Differential test runner (`test_randomized_differential_mvcc_crud_and_reopen`) runs 100 deterministic pseudo-random operations (inserts, updates, deletes, aborts, commits, index lookups, scans, reopen/crash cycles) with a fixed seed (424242) and compares canonical row multisets after every step.
- Pathological distributions verified:
  - Deep version chain (50 sequential updates on a single row) with historical snapshots verified at every step.
  - Delete followed by primary-key reuse across transactions.
  - Frequently changing indexed values and NULLs.

---

## 10. Plan Cache and Prepared Statement Snapshot Independence

- Physical execution plans in `PlanCache` are verified to be strictly independent of transaction and snapshot state.
- `PlanCacheKey` comprises SQL text, parameter types, schema/catalog generation, index generation, statistics generation, and resource planning signatures.
- Tested:
  - Transaction 1 executes prepared statement (cache miss).
  - Transaction 2 updates the data and commits.
  - Transaction 3 executes the identical prepared statement (cache hit!) and observes Transaction 3's snapshot (updated row).
  - Index lifecycle mutation (`CREATE INDEX`) increments `index_generation`, properly invalidating cached plans.

---

## 11. Known Limitations & Explicitly Deferred Features

The following capabilities were explicitly evaluated across phases:
1. **Concurrent Transactions & Multi-Threading**: Single-threaded transaction execution is maintained. No concurrent transaction execution, lock manager, or deadlock detection is introduced.
2. **Historical Version Reclamation (Vacuum)**: *Implemented in Phase 70.* Snapshot-safe version reclamation, version chain pruning, overflow page recycling, empty record page recycling, and secondary/covering index garbage collection are fully supported via `db.vacuum()` and `docs/MVCC_VACUUM.md`.
3. **Transaction Conflict Detection & Commit-Time Validation**: *Implemented in Phase 71.* Formal write-intent tracking (`RowWriteIntent`), write-set validation against committed heads, 6 structured conflict categories (`WriteWriteConflict`, `StaleVersionWrite`, `ConcurrentLogicalKeyChange`, `UniqueIndexConflict`, `InvalidTransactionState`, `WriteSetValidationFailure`), and atomic failure rollbacks are documented in `docs/MVCC_CONFLICTS.md`.
4. **Serializable Snapshot Isolation (SSI)**: Serializable snapshot isolation (predicate locks, rw-antidependencies) is deferred to future isolation phases.
