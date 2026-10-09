# MVCC Snapshot Visibility & Commit Sequence Architecture

## 1. Overview

TreeDB Phase 68 implements deterministic snapshot visibility for multi-version concurrency control (MVCC). By decoupling logical commit sequences (`CommitSeq`) from physical write-ahead log sequences (`Lsn`), TreeDB provides repeatable read and read committed snapshot isolation without locks or background threads.

---

## 2. Monotonic Commit Sequences (`CommitSeq`)

A `CommitSeq` is a 64-bit monotonically increasing integer representing logical transaction ordering:
- **Separation from LSN**: While `Lsn` tracks physical byte offsets in the WAL file, `CommitSeq` strictly orders transactional state changes.
- **Allocation Invariant**: Each committed transaction allocates a strictly greater `CommitSeq`. Aborted transactions do not allocate or advance `CommitSeq`.
- **Durability**: The highest durable commit sequence is stored on Page 0 in `DbHeader::max_commit_seq` and maintained in `BufferPool`. Upon restart, crash recovery re-establishes `next_commit_seq = max_commit_seq + 1`, guaranteeing that commit sequences never regress or collide across reboots.

---

## 3. Snapshot Abstraction

A `Snapshot` represents an immutable point-in-time view of the database:
- `SnapshotId`: Unique 64-bit monotonically allocated identifier for tracking and diagnostic registry purposes.
- `as_of_commit_seq`: The upper commit horizon visible to the snapshot. Any mutation committed with `CommitSeq > as_of_commit_seq` is considered in the future and invisible.

### Snapshot Registry
`SnapshotRegistry` tracks active snapshots across active transactions in $O(1)$ time, calculating `oldest_active_snapshot()` to define the retention horizon for future vacuum implementations.

---

## 4. Formal Visibility Rules (`MvccVisibilityPolicy`)

A version with `xmin`, optional `xmax`, `created_at`, and `deleted_at` is evaluated against a reader transaction `reader_txn` and snapshot `snapshot` using the following deterministic decision tree:

### 1. Transaction-Local Visibility (Own Writes)
If `xmin == reader_txn` (and `reader_txn != NULL_TXN`):
- If `xmax == Some(reader_txn)`: The version was created and subsequently deleted within the current active transaction $\rightarrow$ **INVISIBLE**.
- Otherwise: The version was created by the active transaction $\rightarrow$ **VISIBLE** (read-your-own-writes).

### 2. Foreign Uncommitted Writes (Dirty Read Protection)
If `xmin != reader_txn`:
- If `created_at == None` (and `xmin != NULL_TXN`): The version is uncommitted by another transaction $\rightarrow$ **INVISIBLE**.

### 3. Commit Horizon Enforcement
- If `created_at > snapshot.as_of_commit_seq`: The version committed after the snapshot horizon $\rightarrow$ **INVISIBLE** (future write).

### 4. Deletion Horizon Enforcement
- If `xmax == Some(reader_txn)`: The version was deleted by the active transaction $\rightarrow$ **INVISIBLE**.
- If `deleted_at` is present:
  - If `deleted_at <= snapshot.as_of_commit_seq`: The version was deleted and committed before or at the snapshot horizon $\rightarrow$ **INVISIBLE**.
  - If `deleted_at > snapshot.as_of_commit_seq`: The version was deleted after the snapshot horizon $\rightarrow$ **VISIBLE** (was still alive at snapshot time).
- Otherwise $\rightarrow$ **VISIBLE**.

---

## 5. Isolation Levels

### Read Committed (Default)
Under `IsolationLevel::ReadCommitted`, each statement captures a fresh statement snapshot via `txn.statement_snapshot()`. Newly committed mutations from other transactions between statements become immediately visible.

### Repeatable Read
Under `IsolationLevel::RepeatableRead`, the snapshot captured at `db.begin()` remains immutable throughout the lifetime of the transaction. Successive queries observe identical data regardless of other concurrent commits.

---

## 6. Exact Version Visibility vs. Chain Traversal

In Phase 69, visibility checking is separated into two modes:
1. **Chain Traversal (`RecordStore::read_visible`)**:
   Used for primary-key lookups and sequential table scans starting from the logical head version. It follows `prev_version` links backward until finding a visible version.
2. **Exact Version Visibility (`RecordStore::is_version_visible` / `read_exact_version_if_visible`)**:
   Used for secondary index lookups and covering index scans. Since each secondary index entry points to a specific physical version (`VersionId`), the engine evaluates whether that exact version is visible to the transaction snapshot without traversing `prev_version` links. This prevents version confusion, duplicate rows, and covering payload leakage.

---

## 7. Plan Cache and Prepared Statement Snapshot Independence

Physical execution plans cached in `PlanCache` are strictly independent of transaction and snapshot state:
- `PlanCacheKey` comprises SQL text, normalized parameter types, schema/catalog generation, index generation, statistics generation, and resource planning signatures.
- `PlanCacheKey` and physical plans contain **no** `TxnId`, `SnapshotId`, or `CommitSeq`.
- A single cached physical plan can be executed sequentially or concurrently across distinct transactions; every execution evaluates visibility using the active transaction's runtime snapshot.

---

## 8. Resource Accounting & Metrics

The execution engine monitors MVCC version traversal via `ExecutionResourceManager` and `QueryExecutionMetrics`:
- `versions_examined`: Total number of physical version records inspected.
- `max_chain_depth`: Deepest version chain traversed during query execution.
- `historical_versions_skipped`: Number of historical versions skipped due to invisibility.
- `version_reads`: Number of physical version records read from disk/cache.
- `overflow_pages_read`: Number of overflow pages fetched for multi-page versions.
- `corrupt_chains_detected`: Count of cycles or corrupt links intercepted.

---

## 9. Phase 70: Interaction with the Vacuum Reclamation Horizon

In Phase 70, snapshot visibility forms the primary safety predicate for garbage collection:
- `ReclamationHorizon`: $\min \{ s.\text{as\_of\_commit\_seq} \mid s \in \text{active\_snapshots} \}$.
- Versions required by ANY active snapshot according to `MvccVisibilityPolicy` are strictly protected from reclamation.
- Only versions whose deletion was committed at or before the reclamation horizon ($d_{\text{seq}} \le \text{Horizon}$) and are unneeded by every active snapshot are eligible for pruning.
- See [MVCC_VACUUM.md](file:///d:/Projects/TreeDB/docs/MVCC_VACUUM.md) for complete details.


