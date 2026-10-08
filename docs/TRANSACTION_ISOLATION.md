# TreeDB — Transaction Isolation Contract

This document defines TreeDB's transaction isolation architecture, documenting:
1. The currently enforced single-version transactional model.
2. The prepared snapshot and isolation architecture introduced in Phase 67.
3. The future MVCC contracts and isolation levels scheduled for subsequent phases.

---

## 1. Architectural Distinction

TreeDB explicitly distinguishes three phases of its transaction evolution:

| Capability | Current Status (Phase 67) | Target Status (Future MVCC) |
| :--- | :--- | :--- |
| **Storage Model** | Single-version (in-place slotted pages, WAL-protected) | Multi-version (version headers, tuple chains, vacuum) |
| **Concurrency** | Single-threaded, single active transaction | Concurrent reader/writer threads, lock-free reads |
| **Isolation Levels** | `IsolationLevel` defined & prepared; single-version semantics enforced | `READ COMMITTED`, `REPEATABLE READ`, `SERIALIZABLE` enforced |
| **Visibility Policy** | `CurrentVersionVisibility` baseline | `MvccVisibilityPolicy` with version chain traversal |
| **Snapshot Handling** | Logical `Snapshot` & `SnapshotRegistry` tracked | Statement/transaction snapshot visibility filtering in `RecordStore` |
| **Conflict Detection** | Abort on runtime error / constraint violation | Write-write conflict detection, SSI (Serializable Snapshot Isolation) |

---

## 2. Three Distinct Identifiers

To prevent conflation between crash recovery, transaction identity, and visibility ordering, TreeDB establishes three independent monotonically allocated sequences:

```text
+-------------------+-------------------------------------------------------------+
| Identifier        | Purpose & Architectural Role                                |
+-------------------+-------------------------------------------------------------+
| TxnId             | Unique transaction identity; allocated at transaction begin |
|                   | Recorded in WAL log records for transaction association.    |
+-------------------+-------------------------------------------------------------+
| Lsn               | Log Sequence Number; physical WAL and crash recovery order. |
|                   | Determines redo/undo replay position during restart.        |
+-------------------+-------------------------------------------------------------+
| CommitSeq         | Logical commit sequence; allocated upon transaction commit. |
|                   | Governs logical visibility ordering for MVCC snapshots.     |
+-------------------+-------------------------------------------------------------+
```

### Critical Invariants:
1. **LSN and CommitSeq are never conflated:** WAL crash recovery operates strictly on `Lsn`. Transaction visibility operates on `CommitSeq`.
2. **CommitSeq allocation is monotonic and committed-only:** Aborted transactions do NOT advance the database's committed `CommitSeq`.
3. **Snapshot isolation horizon:** A snapshot captures `as_of_commit_seq = CommitSeq(N)`. Any transaction committed at or prior to $N$ is eligible to be visible under future MVCC rules.

---

## 3. Isolation Levels

TreeDB defines three SQL isolation levels via `IsolationLevel`:

```rust
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}
```

### Level Status Matrix

| Isolation Level | Phase 67 Status | Currently Enforced Behavior | Future MVCC Contract |
| :--- | :--- | :--- | :--- |
| **`ReadCommitted`** | `PartiallyPrepared` | Single-version transaction. Writes are immediately visible to self; committed writes visible to subsequent transactions. | Each statement acquires a fresh statement snapshot current at statement start. Never observes dirty writes from uncommitted transactions. |
| **`RepeatableRead`** | `PartiallyPrepared` | Single-version transaction. Uncommitted writes visible to self; committed writes visible to subsequent transactions. | All statements observe the immutable transaction snapshot captured at `BEGIN`. Reads are repeatable across queries within the same transaction. |
| **`Serializable`** | `DefinedNotEnforced` | Single-version transaction. Same single-threaded execution as `ReadCommitted`. | Transactions execute as if in a strict serial order. Conflict detection (SSI or strict 2PL) detects and aborts read-write anti-dependencies. |

---

## 4. Transaction Lifecycle State Machine

TreeDB transactions follow an explicit, deterministic state machine:

```text
         Created
            |
            v
          Active
         /      \
        /        \
       v          v
   Committed    Aborted
```

### State Transitions:
- `Created -> Active`: Transaction initialization.
- `Active -> Committed`: Transaction mutations flushed to WAL, `CommitSeq` allocated, snapshot released, catalog/cache invalidated if DDL was executed.
- `Active -> Aborted`: Physical page mutations rolled back via CLRs, snapshot released without advancing `CommitSeq`, original metadata restored.
- **Drop auto-abort:** If an `Active` transaction handle is dropped without calling `commit()`, its `Drop` implementation automatically executes rollback and deregisters its snapshot.

### Rejection of Invalid Transitions:
- `Committed -> commit`: Returns `Error::InvalidTransactionState`.
- `Committed -> abort`: Returns `Error::InvalidTransactionState`.
- `Aborted -> commit`: Returns `Error::InvalidTransactionState`.
- `Aborted -> abort`: Returns `Error::InvalidTransactionState`.
- `Created -> commit`: Returns `Error::InvalidTransactionState`.
- Reads or mutations on `Committed` or `Aborted` handles return structured errors (`TransactionAlreadyCommitted`, `TransactionAlreadyAborted`).

---

## 5. Centralized Visibility Policy

Visibility logic is centralized through the `VisibilityPolicy` trait:

```rust
pub trait VisibilityPolicy: Send + Sync {
    fn is_visible(
        &self,
        creator: TxnId,
        commit_seq: Option<CommitSeq>,
        snapshot: &Snapshot,
    ) -> bool;
}
```

### Current Baseline: `CurrentVersionVisibility`
Codifies today's single-version transaction semantics:
1. **Own-write visibility:** Uncommitted or committed writes by the reading transaction are visible to itself.
2. **Committed writes:** Writes committed at or before the reader's snapshot commit horizon (`commit_seq <= snapshot.as_of_commit_seq`) are visible.
3. **Dirty read protection:** Uncommitted writes belonging to other transactions (`commit_seq.is_none() && creator != reader`) are invisible.

Query execution operators consume visibility through `ExecutionContext.isolation_context` rather than inventing operator-specific visibility logic.

---

## 6. Execution Context & Physical Plan Isolation

Physical query execution is decoupled from transaction state:

1. **Physical Plan Reusability:** A `PhysicalPlan` is an immutable, transaction-independent execution strategy. It contains **no** `TxnId`, `SnapshotId`, or `CommitSeq`.
2. **Plan Caching:** `PlanCacheKey` and `PhysicalPlanFingerprint` depend strictly on query syntax, parameter planning signatures, and resource budgets. Cached plans can be executed by any transaction without cache invalidation or state leakage.
3. **Execution Tree Isolation:** Every query invocation constructs a fresh execution operator tree governed by a fresh `ExecutionContext` carrying the calling transaction's `IsolationContext`.
4. **Prepared Statement Safety:** Prepared statements retain parameterized AST structures and are executed across differing transaction contexts without leaking snapshot state.
