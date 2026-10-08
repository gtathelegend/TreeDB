//! Canonical Transaction Isolation, Snapshot Architecture & MVCC Preparation.
//!
//! Formalizes TreeDB's transaction isolation contract prior to physical MVCC record versioning:
//! 1. Explicit `IsolationLevel` enum (`ReadCommitted`, `RepeatableRead`, `Serializable`).
//! 2. Immutable, logical `Snapshot` abstraction with monotonic `SnapshotId`.
//! 3. Monotonic logical commit sequence (`CommitSeq`), strictly separated from physical WAL `Lsn`.
//! 4. Unified `IsolationContext` carrying isolation level, transaction ID, and snapshot into query execution.
//! 5. Baseline `VisibilityPolicy` (`CurrentVersionVisibility`) codifying single-version visibility rules.
//! 6. `TransactionWriteSet` tracking transaction-local mutations for future MVCC bookkeeping.
//! 7. Conceptual `ReadSet` prepared for future serializable conflict detection (SSI/2PL).
//! 8. Database-level `SnapshotRegistry` tracking active snapshots and calculating `oldest_active_snapshot()`.

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use crate::engine::transaction::TxnState;
use crate::index::RecordId;
use crate::storage::page::PageId;
use crate::wal::lsn::TxnId;

/// Explicit SQL transaction isolation levels supported or prepared in TreeDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IsolationLevel {
    /// Read Committed isolation.
    ///
    /// - Future MVCC contract: Each statement observes a fresh snapshot captured at statement start.
    /// - Phase 67 status: Partially prepared (statement snapshot boundary prepared; single-version engine).
    #[default]
    ReadCommitted,

    /// Repeatable Read isolation.
    ///
    /// - Future MVCC contract: All statements observe the immutable snapshot captured at transaction begin.
    /// - Phase 67 status: Partially prepared (transaction snapshot abstraction prepared; single-version engine).
    RepeatableRead,

    /// Serializable isolation.
    ///
    /// - Future MVCC contract: Transactions execute as if sequentially ordered, with serializable conflict detection.
    /// - Phase 67 status: Defined; conflict detection not yet enforced.
    Serializable,
}

impl IsolationLevel {
    /// Returns the canonical human-readable name of this isolation level.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }

    /// Returns the implementation and enforcement status of this isolation level in Phase 67.
    pub const fn status(&self) -> IsolationStatus {
        match self {
            Self::ReadCommitted => IsolationStatus::PartiallyPrepared,
            Self::RepeatableRead => IsolationStatus::PartiallyPrepared,
            Self::Serializable => IsolationStatus::DefinedNotEnforced,
        }
    }
}

impl fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Implementation status classification for transaction isolation levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationStatus {
    /// Level is fully defined in the contract.
    Defined,
    /// Level abstraction is implemented and snapshot boundaries are wired, but single-version storage is active.
    PartiallyPrepared,
    /// Level is fully enforced by multi-version storage.
    Implemented,
    /// Level is defined in API but conflict detection / isolation guarantees are not yet enforced.
    DefinedNotEnforced,
}

impl fmt::Display for IsolationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defined => write!(f, "Defined"),
            Self::PartiallyPrepared => write!(f, "Partially Prepared"),
            Self::Implemented => write!(f, "Implemented"),
            Self::DefinedNotEnforced => write!(f, "Defined (Not Enforced)"),
        }
    }
}

/// Monotonically allocated logical snapshot identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SnapshotId(pub u64);

impl SnapshotId {
    /// Special null or uninitialized snapshot identifier.
    pub const NULL: Self = Self(0);

    /// Constructs a new `SnapshotId`.
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Returns the raw integer value.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Returns the next sequential snapshot ID.
    pub const fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SnapshotId({})", self.0)
    }
}

/// Monotonically allocated logical commit sequence number.
///
/// Distinct from WAL `Lsn`:
/// - `Lsn`: Physical WAL recovery and crash ordering.
/// - `CommitSeq`: Logical transaction commit and visibility ordering for MVCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CommitSeq(pub u64);

impl CommitSeq {
    /// Special zero / uncommitted commit sequence.
    pub const ZERO: Self = Self(0);

    /// Constructs a new `CommitSeq`.
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Returns the raw integer value.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Returns the next sequential commit sequence.
    pub const fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for CommitSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitSeq({})", self.0)
    }
}

/// Immutable, deterministic point-in-time snapshot abstraction.
///
/// A `Snapshot` is cheap to copy, transaction-owned, and independent of physical page IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Snapshot {
    /// Unique logical snapshot identifier.
    pub snapshot_id: SnapshotId,
    /// Logical commit sequence horizon observed by this snapshot.
    /// Transactions committed at or before this sequence are visible.
    pub as_of_commit_seq: CommitSeq,
}

impl Snapshot {
    /// Constructs a new immutable snapshot.
    pub const fn new(snapshot_id: SnapshotId, as_of_commit_seq: CommitSeq) -> Self {
        Self {
            snapshot_id,
            as_of_commit_seq,
        }
    }
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Snapshot(id={}, as_of={})",
            self.snapshot_id.0, self.as_of_commit_seq.0
        )
    }
}

/// Immutable isolation and visibility context passed into query execution pipelines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationContext {
    /// Configured transaction isolation level.
    pub isolation_level: IsolationLevel,
    /// Owning transaction identifier.
    pub txn_id: TxnId,
    /// Point-in-time snapshot observed by this transaction or statement.
    pub snapshot: Snapshot,
}

impl IsolationContext {
    /// Creates a new `IsolationContext`.
    pub const fn new(isolation_level: IsolationLevel, txn_id: TxnId, snapshot: Snapshot) -> Self {
        Self {
            isolation_level,
            txn_id,
            snapshot,
        }
    }

    /// Creates a default `IsolationContext` for a transaction under `ReadCommitted` isolation.
    pub const fn for_txn(txn_id: TxnId) -> Self {
        Self {
            isolation_level: IsolationLevel::ReadCommitted,
            txn_id,
            snapshot: Snapshot::new(SnapshotId(txn_id.0), CommitSeq::ZERO),
        }
    }

    /// Creates an `IsolationContext` for a transaction with an explicit isolation level and snapshot.
    pub const fn for_txn_with_level(
        txn_id: TxnId,
        isolation_level: IsolationLevel,
        snapshot: Snapshot,
    ) -> Self {
        Self {
            isolation_level,
            txn_id,
            snapshot,
        }
    }
}

/// Transaction commit metadata describing lifecycle outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionCommitMetadata {
    /// Unique transaction identifier.
    pub txn_id: TxnId,
    /// Assigned logical commit sequence if committed.
    pub commit_seq: Option<CommitSeq>,
    /// Lifecycle state of the transaction.
    pub state: TxnState,
}

impl TransactionCommitMetadata {
    /// Constructs metadata for an active transaction.
    pub const fn active(txn_id: TxnId) -> Self {
        Self {
            txn_id,
            commit_seq: None,
            state: TxnState::Active,
        }
    }

    /// Constructs metadata for a committed transaction.
    pub const fn committed(txn_id: TxnId, commit_seq: CommitSeq) -> Self {
        Self {
            txn_id,
            commit_seq: Some(commit_seq),
            state: TxnState::Committed,
        }
    }

    /// Constructs metadata for an aborted transaction.
    pub const fn aborted(txn_id: TxnId) -> Self {
        Self {
            txn_id,
            commit_seq: None,
            state: TxnState::Aborted,
        }
    }
}

/// Centralized contract defining whether a record or version is visible to a reader.
pub trait VisibilityPolicy: Send + Sync {
    /// Evaluates visibility given the creator transaction ID, optional commit sequence, and reader snapshot.
    fn is_visible(
        &self,
        creator: TxnId,
        commit_seq: Option<CommitSeq>,
        snapshot: &Snapshot,
    ) -> bool;
}

/// Baseline visibility policy codifying TreeDB's single-version transaction semantics.
///
/// Rules:
/// 1. Own writes (creator matches reader) are always visible.
/// 2. Committed writes prior to or at the snapshot commit horizon are visible.
/// 3. Uncommitted writes from other transactions are invisible.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentVersionVisibility;

impl VisibilityPolicy for CurrentVersionVisibility {
    fn is_visible(
        &self,
        creator: TxnId,
        commit_seq: Option<CommitSeq>,
        snapshot: &Snapshot,
    ) -> bool {
        if creator == crate::wal::lsn::NULL_TXN {
            return true;
        }
        if let Some(cseq) = commit_seq {
            cseq.0 <= snapshot.as_of_commit_seq.0
        } else {
            false
        }
    }
}

impl CurrentVersionVisibility {
    /// Evaluates visibility in the context of a specific `IsolationContext`.
    pub fn is_visible_to_context(
        &self,
        creator: TxnId,
        commit_seq: Option<CommitSeq>,
        ctx: &IsolationContext,
    ) -> bool {
        if creator == ctx.txn_id {
            // Own writes (uncommitted or committed) are visible to self
            true
        } else if let Some(cseq) = commit_seq {
            // Committed prior to or at snapshot horizon
            cseq.0 <= ctx.snapshot.as_of_commit_seq.0
        } else {
            // Uncommitted writes from other transactions are invisible
            false
        }
    }
}

/// Multi-version concurrency control (MVCC) snapshot visibility evaluator.
///
/// Rules:
/// 1. Own writes:
///    - If created by reader: visible, UNLESS deleted by reader (in which case invisible).
/// 2. Other transactions:
///    - If uncommitted by creator: invisible (dirty read protection).
///    - If committed by creator after snapshot commit horizon: invisible (future write).
///    - If deleted by reader: invisible.
///    - If deleted and committed by another transaction at or before snapshot horizon: invisible (deleted in past).
///    - Otherwise: visible!
#[derive(Debug, Clone, Copy, Default)]
pub struct MvccVisibilityPolicy;

impl MvccVisibilityPolicy {
    /// Determines whether a version is visible to a reader with `reader_txn` and `snapshot`.
    pub fn is_visible(
        xmin: TxnId,
        xmax: Option<TxnId>,
        created_at: Option<CommitSeq>,
        deleted_at: Option<CommitSeq>,
        reader_txn: Option<TxnId>,
        snapshot: &Snapshot,
    ) -> bool {
        // 1. Transaction-Local Visibility (Own writes)
        if let Some(rt) = reader_txn {
            if xmin == rt && rt != crate::wal::lsn::NULL_TXN {
                // Created by self in this active transaction
                // If deleted by self in this active transaction -> invisible to self
                if xmax == Some(rt) {
                    return false;
                }
                // Otherwise visible to self (read-your-own-writes)
                return true;
            }
        }

        // 2. Created by another transaction (or bootstrap/system)
        // Must be committed to be visible to others (no dirty reads)
        let created_seq = match created_at {
            Some(seq) => seq,
            None => {
                // If created by another uncommitted transaction -> invisible
                if xmin == crate::wal::lsn::NULL_TXN {
                    CommitSeq::ZERO
                } else {
                    return false;
                }
            }
        };

        // Must have committed at or before the reader's snapshot horizon
        if created_seq.0 > snapshot.as_of_commit_seq.0 {
            return false;
        }

        // 3. Deletion visibility
        // Did the reader's own transaction delete this version?
        if let Some(rt) = reader_txn {
            if xmax == Some(rt) && rt != crate::wal::lsn::NULL_TXN {
                // Deleted by self -> invisible to self
                return false;
            }
        }

        // Was it deleted and committed by another transaction?
        if let Some(deleted_seq) = deleted_at {
            // If the deletion committed at or before the reader's snapshot horizon -> deleted and invisible
            if deleted_seq.0 <= snapshot.as_of_commit_seq.0 {
                return false;
            }
        }

        // Otherwise: created in the past, and not yet deleted in this snapshot horizon
        true
    }
}

/// Transaction write-set tracking local modifications for MVCC bookkeeping and rollback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionWriteSet {
    inserted: HashSet<RecordId>,
    updated: HashSet<RecordId>,
    deleted: HashSet<RecordId>,
    modified_tables: HashSet<String>,
}

impl TransactionWriteSet {
    /// Creates an empty `TransactionWriteSet`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an inserted record.
    pub fn record_insert(&mut self, rid: RecordId) {
        self.inserted.insert(rid);
    }

    /// Records an updated record, tracking the pre-update and post-update `RecordId`s.
    pub fn record_update(&mut self, old_rid: RecordId, new_rid: RecordId) {
        self.updated.insert(old_rid);
        self.inserted.insert(new_rid);
    }

    /// Records a deleted record.
    pub fn record_delete(&mut self, rid: RecordId) {
        self.deleted.insert(rid);
    }

    /// Records a modified table name.
    pub fn record_table_mutation(&mut self, table: &str) {
        self.modified_tables.insert(table.to_string());
    }

    /// Returns `true` if the specified `RecordId` was affected by this transaction.
    pub fn contains(&self, rid: RecordId) -> bool {
        self.inserted.contains(&rid) || self.updated.contains(&rid) || self.deleted.contains(&rid)
    }

    /// Returns `true` if `rid` was inserted by this transaction.
    pub fn is_inserted(&self, rid: RecordId) -> bool {
        self.inserted.contains(&rid)
    }

    /// Returns `true` if `rid` was updated by this transaction.
    pub fn is_updated(&self, rid: RecordId) -> bool {
        self.updated.contains(&rid)
    }

    /// Returns `true` if `rid` was deleted by this transaction.
    pub fn is_deleted(&self, rid: RecordId) -> bool {
        self.deleted.contains(&rid)
    }

    /// Returns the number of inserted records.
    pub fn inserted_count(&self) -> usize {
        self.inserted.len()
    }

    /// Returns the number of updated records.
    pub fn updated_count(&self) -> usize {
        self.updated.len()
    }

    /// Returns the number of deleted records.
    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }

    /// Returns a reference to all inserted record IDs.
    pub fn inserted_records(&self) -> &HashSet<RecordId> {
        &self.inserted
    }

    /// Returns a reference to all updated pre-image record IDs.
    pub fn updated_records(&self) -> &HashSet<RecordId> {
        &self.updated
    }

    /// Returns a reference to all deleted record IDs.
    pub fn deleted_records(&self) -> &HashSet<RecordId> {
        &self.deleted
    }

    /// Returns the total number of record mutations.
    pub fn total_mutations(&self) -> usize {
        self.inserted.len() + self.updated.len() + self.deleted.len()
    }

    /// Returns `true` if no mutations have been recorded.
    pub fn is_empty(&self) -> bool {
        self.inserted.is_empty()
            && self.updated.is_empty()
            && self.deleted.is_empty()
            && self.modified_tables.is_empty()
    }

    /// Clears all recorded writes.
    pub fn clear(&mut self) {
        self.inserted.clear();
        self.updated.clear();
        self.deleted.clear();
        self.modified_tables.clear();
    }
}

/// Conceptual transaction read-set abstraction prepared for future serializable conflict detection (SSI/2PL).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSet {
    /// Records accessed by point or range queries.
    pub records: HashSet<RecordId>,
    /// Pages read during index or table scans.
    pub pages: HashSet<PageId>,
    /// Tables referenced in queries.
    pub tables: HashSet<String>,
}

impl ReadSet {
    /// Creates an empty `ReadSet`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a read access to a specific record.
    pub fn record_read(&mut self, rid: RecordId) {
        self.records.insert(rid);
    }

    /// Records a read access to a specific page.
    pub fn record_page_read(&mut self, page_id: PageId) {
        self.pages.insert(page_id);
    }

    /// Records a read access to a specific table.
    pub fn record_table_read(&mut self, table: &str) {
        self.tables.insert(table.to_string());
    }

    /// Returns `true` if no reads have been recorded.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.pages.is_empty() && self.tables.is_empty()
    }

    /// Clears all recorded reads.
    pub fn clear(&mut self) {
        self.records.clear();
        self.pages.clear();
        self.tables.clear();
    }
}

/// Database-level snapshot registry tracking active transactions and oldest active snapshot horizon.
#[derive(Debug, Clone)]
pub struct SnapshotRegistry {
    active_snapshots: BTreeSet<SnapshotId>,
    next_snapshot_id: u64,
}

impl Default for SnapshotRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotRegistry {
    /// Creates a new `SnapshotRegistry`.
    pub fn new() -> Self {
        Self {
            active_snapshots: BTreeSet::new(),
            next_snapshot_id: 1,
        }
    }

    /// Registers a new active snapshot observing commit sequence `as_of`.
    pub fn register(&mut self, as_of: CommitSeq) -> Snapshot {
        let sid = SnapshotId(self.next_snapshot_id);
        self.next_snapshot_id = self.next_snapshot_id.saturating_add(1);
        self.active_snapshots.insert(sid);
        Snapshot::new(sid, as_of)
    }

    /// Releases an active snapshot upon transaction commit, abort, or statement boundary.
    pub fn release(&mut self, sid: SnapshotId) {
        self.active_snapshots.remove(&sid);
    }

    /// Returns the oldest active snapshot ID if any snapshots are active.
    ///
    /// Runs in O(1) time using `BTreeSet::iter().next()`.
    pub fn oldest_active_snapshot(&self) -> Option<SnapshotId> {
        self.active_snapshots.iter().next().copied()
    }

    /// Returns the number of currently active registered snapshots.
    pub fn active_count(&self) -> usize {
        self.active_snapshots.len()
    }

    /// Returns `true` if the specified snapshot ID is currently registered and active.
    pub fn is_active(&self, sid: SnapshotId) -> bool {
        self.active_snapshots.contains(&sid)
    }
}
