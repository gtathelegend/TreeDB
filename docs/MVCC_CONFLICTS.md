# Transaction Conflict Detection & Commit-Time Validation

TreeDB implements a formal transaction conflict-detection and commit-validation layer on top of its multi-version concurrency control (MVCC) architecture.

This ensures that transactions cannot silently commit writes based on stale or invalid assumptions about the versions they modify.

> [!NOTE]
> **Single-Writer Invariant**: In accordance with Phase 71 constraints, concurrent writer execution remains disabled. No lock manager, deadlock detector, multithreading, or Serializable Snapshot Isolation (SSI) is implemented. All validation rules enforce correctness for interleaved transaction snapshots and commit sequences before concurrent execution is introduced.

---

## 1. Conflict Categories

The conflict contract is formalized in [	reedb::engine::isolation::TransactionConflict](../src/engine/isolation.rs) and exposed via 	reedb::error::Error::TransactionConflict.

The engine distinguishes six distinct categories:

1. **WriteWriteConflict**:
   A transaction attempts to update or delete a logical row whose latest committed version no longer matches the version it originally targeted.
   * *Diagnostic Context*: 	able, key, xpected_version, observed_version, 	xn_id.

2. **StaleVersionWrite**:
   A transaction attempts to modify or delete a historical version that has already been superseded by a newer version or deleted before validation.
   * *Diagnostic Context*: 	able, key, 	arget_version, 	xn_id.

3. **ConcurrentLogicalKeyChange**:
   An attempted primary-key operation conflicts with an intervening committed change (for example, inserting a primary key that has been inserted and committed by another transaction).
   * *Diagnostic Context*: 	able, key, 	xn_id.

4. **UniqueIndexConflict**:
   An insert or indexed-key update would violate a unique secondary index constraint under TreeDB's uniqueness semantics.
   * *Diagnostic Context*: 	able, index_name, key, 	xn_id.

5. **InvalidTransactionState**:
   The transaction attempts to commit after aborting, commit twice, perform operations on an already terminated transaction, or perform invalid state transitions.
   * *Diagnostic Context*: 	xn_id, state.

6. **WriteSetValidationFailure**:
   The transaction's accumulated write intent cannot be safely validated or applied at commit time due to internal consistency failures or pre-commit checks.
   * *Diagnostic Context*: eason.

Every category produces structured errors with descriptive fields and methods Error::is_conflict() and Error::conflict_category().

---

## 2. Write-Set Representation

Write intent is tracked in TransactionWriteSet using strongly typed RowWriteIntent records:

`ust
pub struct RowWriteIntent {
    pub table: String,
    pub key: Key,
    pub key_repr: String,
    pub target_version: Option<VersionId>,
    pub new_version: Option<VersionId>,
    pub snapshot_commit_seq: CommitSeq,
    pub op_kind: WriteOpKind, // Insert, Update, Delete
}
`

### Key Properties:
- **Logical Identity**: Rows are identified by (String, Key) where Key is the canonical byte representation of the primary key.
- **Strongly Typed Identifiers**: TxnId, CommitSeq, VersionId, SnapshotId, RecordId, and PageId are distinct types.
- **Physical Independence**: The write set representation is completely independent of B+ Tree layout, page splits, or index traversal order.
- **Deterministic Intra-Transaction Behavior**:
  - Updating a row twice in the same transaction updates the existing intent's 
ew_version while keeping the original 	arget_version.
  - Inserting and then deleting a row within the same transaction marks the final intent as deleted with 	arget_version: None. At commit validation, no pre-existing version is expected.
  - Deleting and re-inserting within the same transaction preserves the original 	arget_version for conflict checking while updating the operation kind to Update.

---

## 3. Commit-Validation Ordering

TreeDB enforces a strict, multi-step commit-validation pipeline prior to any physical durability or commit record logging:

`
[Transaction Active]
       │
       ▼
1. State Check: Confirm transaction is Active (reject Committed/Aborted)
       │
       ▼
2. Write-Set Integrity: Verify non-empty write-set has recorded intents
       │
       ▼
3. Head Version Verification:
   For each RowWriteIntent:
     - Fetch current committed head version from primary index
     - Update / Delete: assert head version == target_version
     - Insert: assert head is absent OR was committed deleted
       │
       ▼
4. Secondary Unique-Index Validation:
   For each unique secondary index on the table:
     - Check index entries for new versions
     - Confirm no other live committed row shares the unique key
       │
       ▼
5. WAL-Protected Commit Execution:
   - Assign CommitSeq and log Commit record to WAL
   - Mark in-memory version metadata as committed
   - Update transaction state to Committed
`

If validation fails at any point in steps 1–4, the transaction is **automatically aborted**, all uncommitted version allocations are rolled back via WAL undo / internal rollback, and a structured TransactionConflict error is returned.

---

## 4. Snapshot Visibility vs. Write Validation

Conflict detection is strictly decoupled from snapshot visibility:

- **Snapshot Visibility** determines what rows an active transaction can observe during reads. A transaction reading with snapshot S sees versions where xmin <= S and xmax > S (or uncommitted).
- **Write Validation** determines whether a transaction's proposed mutations are valid against the **latest committed state** (C_head), regardless of what was visible at snapshot S.

A transaction reading an older version under its snapshot is permitted to read it, but if another transaction commits a modification to that row before the first transaction commits, the first transaction's write is rejected with WriteWriteConflict or StaleVersionWrite.

---

## 5. Unique Indexes and Uniqueness Semantics

Unique constraints on primary and secondary indexes follow these formal rules:

1. **Live Row Scope**: Uniqueness is defined strictly in terms of live (non-deleted, committed) rows.
2. **Historical Versions**: Historical versions belonging to older snapshots or superseding chains do not constitute uniqueness violations.
3. **Null Handling**: Consistent with TreeDB conventions, unique secondary indexes evaluate NULL == NULL as duplicate keys unless explicitly configured otherwise.
4. **Access-Path Invariance**: Unique validation evaluates row data regardless of whether access is performed via full table scan, index range scan, or point lookup.
5. **Vacuum Safety**: Obsolete versions reclaimed by vacuum do not alter the uniqueness validation outcome of live committed keys.

---

## 6. Atomic Failure, Rollback, and Recovery

When commit validation detects a conflict:
- **Contract**: The transaction transitions immediately to Aborted.
- **No Partial Commit**: No uncommitted logical mutations are exposed. Uncommitted physical allocations are rolled back or cleaned up.
- **WAL Durability**: The WAL never receives a Commit record for a transaction that failed validation.
- **Crash Recovery**: If the system crashes prior to a Commit record, recovery scans the WAL and rolls back all modifications for uncommitted transactions.
- **Retry Semantics**: TreeDB does not perform automatic retries. The client application receives a structured conflict error and must decide whether to retry from a fresh snapshot.

---

## 7. Interaction with Vacuum & Reclamation

Vacuum reclaims versions based on the **Reclamation Horizon**:
Horizon = min(Active Snapshots' Read Commit Sequences)

- **Preservation of Active Snapshots**: Versions required by active transactions for visibility are never reclaimed.
- **Preservation of Validation Target**: Vacuum only reclaims historical versions committed strictly before the oldest active snapshot.
- **Vacuum Consistency**: Vacuum validation (alidate_vacuum_consistency()) checks that version chain heads, overflow chains, and index entries remain structurally sound after vacuum runs.

---

## 8. Supported Transaction Interleavings & Limitations

### Supported Interleavings:
- Sequential transactions with overlapping read snapshots.
- Read-only transactions concurrent with write transactions.
- Multiple transactions modifying non-overlapping logical rows.
- Transaction aborts, rollbacks, and subsequent retries on fresh snapshots.

### Explicit Limitations (Out of Scope for Phase 71):
- **Concurrent Writers**: Only one active writer transaction may execute mutations at a time.
- **Lock Management**: No row-level or table-level lock managers.
- **Deadlock Detection**: No lock wait graphs or deadlock resolution.
- **Serializable Snapshot Isolation (SSI)**: No tracking of rw-antidependencies or SIREAD locks.
