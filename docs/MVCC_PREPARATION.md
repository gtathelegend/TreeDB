# TreeDB — MVCC Preparation Architecture

This document outlines the architectural bridge between TreeDB's current single-version storage engine and its planned multi-version concurrency control (MVCC) engine.

Phase 67 establishes all abstractions, lifecycle boundaries, metadata models, and isolation contexts required by MVCC **without modifying underlying record storage formats or query results**.

---

## 1. Architectural Progression

```text
Current (Single-Version)
  Transaction -> TxnId -> WAL LSN -> In-Place Slotted Page
        |
        v
Phase 67 (Preparation Bridge)
  Transaction
       |
       +---> IsolationLevel
       +---> Snapshot (SnapshotId, CommitSeq)
       +---> IsolationContext
       +---> VisibilityPolicy (CurrentVersionVisibility)
       +---> TransactionWriteSet
       |
       v
  ExecutionContext -> Pull Execution Pipelines
        |
        v
Future (Full MVCC)
  RecordStore
       |
       +---> Tuple Header [xmin, xmax, commit_seq, prev_version]
       +---> Version Chains (Undo / In-place versions)
       +---> Vacuum & Version Garbage Collection Horizon
```

---

## 2. Storage Boundaries & Separation of Concerns

### B+ Tree Layer
The B+ Tree index remains strictly concerned with indexing canonical search keys:

$$\text{Key} \longrightarrow \text{RecordId}$$

**Critical Architectural Invariant:**
> The B+ Tree must **NOT** be modified to understand MVCC versions or visibility.
> It must never map $\text{Key} \longrightarrow \text{Visible Version}$.
> Index lookups locate the record location (`RecordId`); the record/version layer determines version visibility.

### RecordStore & Version Layer
The future MVCC layer will sit at the `RecordStore` boundary:

```text
B+ Tree Lookup
       |
       v
RecordId (PageId, SlotIdx)
       |
       v
RecordStore / Version Layer
       |
       +---> Version Header:
       |       - creator TxnId (xmin)
       |       - deleter TxnId (xmax)
       |       - commit_seq (visibility sequence)
       |       - prev_record_id (version chain pointer)
       +---> Payload (encoded row tuple)
```

In Phase 67, this boundary is prepared conceptually via `VisibilityPolicy`, `TransactionCommitMetadata`, and `TransactionWriteSet`. The physical page and record binary formats remain strictly unchanged.

---

## 3. Snapshot Registry & Garbage Collection Horizon

Phase 67 introduces the database-level `SnapshotRegistry`.

In future MVCC phases, version vacuuming and garbage collection require answering:
> What is the oldest snapshot currently active anywhere in the database?

Any tuple version obsolete prior to `oldest_active_snapshot()` can be safely pruned, unlinked, and reclaimed.

### In Phase 67:
- `SnapshotRegistry` registers each snapshot on `begin()` in $O(1)$ time.
- `SnapshotRegistry::oldest_active_snapshot()` computes the horizon in $O(1)$ time via `BTreeSet::iter().next()`.
- Active snapshots are automatically deregistered on `commit()`, `abort()`, or handle `Drop`.
- Version garbage collection is intentionally deferred.

---

## 4. Crash Recovery & Checkpoint Implications

### WAL Recovery
TreeDB's WAL recovery uses ARIES algorithms with physiological logging:
- `Begin`, `PageDelta`, `Clr`, `Commit`, `Abort`, `Checkpoint`.
- Recovery uses `Lsn` for replay.
- Active uncommitted transactions at crash time are rolled back using Compensation Log Records (CLRs).

### Visibility Monotonicity
- Logical visibility ordering is dictated by `CommitSeq`, which is strictly separated from `Lsn`.
- Transactions that abort or crash do not advance the committed `CommitSeq`.
- On database restart, active transaction registry is reset, and new transactions continue with monotonic sequence allocation.

---

## 5. Explicit Non-Goals (Intentionally Deferred Work)

Phase 67 is strictly architectural preparation. The following capabilities are **explicitly not implemented** in this phase:

- [ ] Actual row versions or version tuples on disk
- [ ] Version chains or pointer links between tuple revisions
- [ ] Physical version header timestamps or `xmin`/`xmax` storage
- [ ] Snapshot visibility filtering inside `RecordStore::get` or `Table::scan`
- [ ] Vacuum or version garbage collection
- [ ] Row-level locks or row lock managers
- [ ] Page-level shared/exclusive latches
- [ ] Deadlock detection or wait-for graph analysis
- [ ] Concurrent transaction execution or multithreaded workers
- [ ] Serializable Snapshot Isolation (SSI) anti-dependency tracking
- [ ] Write-write conflict detection or write stall mechanisms
- [ ] Two-phase locking (2PL)
- [ ] Phantom protection via predicate locks or index range locks
- [ ] Distributed transaction protocols

All deferred capabilities will be layered onto the isolation and snapshot foundation established in this phase.
