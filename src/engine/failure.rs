//! Deterministic Failure-Injection Framework for Crash and Recovery Correctness Testing.
//!
//! Provides deterministic, named failure boundaries enabling rigorous testing of
//! transaction atomicity, durability, MVCC version chain preservation, WAL recovery,
//! index consistency, and vacuum storage reclamation.
//!
//! Inactive by default with zero production overhead.

/// Deterministic failure boundaries across transaction, storage, WAL, index, and vacuum subsystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageFailurePoint {
    /// Before the first transaction mutation begins.
    BeforeFirstMutation,
    /// After a record version is allocated on a record page.
    AfterRecordVersionAllocated,
    /// After record payload delta has been logged to the WAL.
    AfterRecordPayloadLogged,
    /// After version metadata (xmin/xmax/created_at/deleted_at) has been modified.
    AfterVersionMetadataChanged,
    /// After primary B+ Tree index insertion or modification.
    AfterPrimaryIndexModified,
    /// After secondary index insertion or modification.
    AfterSecondaryIndexModified,
    /// During multi-page overflow page allocation.
    DuringOverflowPageAlloc,
    /// During record-slot reclamation (e.g. vacuum).
    DuringRecordSlotReclaim,
    /// During freelist updates (page allocation or freeing).
    DuringFreelistUpdate,
    /// Immediately before logging the Commit record to the WAL.
    BeforeCommitRecordLogged,
    /// After appending the Commit record to the WAL buffer, but before the required flush.
    AfterCommitRecordLoggedBeforeFlush,
    /// Immediately after the Commit record has been flushed and is durable on disk.
    AfterDurableCommitRecordFlush,
    /// During checkpoint or database header persistence to disk.
    DuringHeaderPersistence,
    /// During vacuum secondary index cleanup.
    DuringVacuumIndexCleanup,
    /// During vacuum record version chain pruning / reclamation.
    DuringVacuumRecordReclaim,
    /// During vacuum freelist updates.
    DuringVacuumFreelistUpdate,
}

impl StorageFailurePoint {
    /// Returns the human-readable canonical name of the failure boundary.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::BeforeFirstMutation => "BeforeFirstMutation",
            Self::AfterRecordVersionAllocated => "AfterRecordVersionAllocated",
            Self::AfterRecordPayloadLogged => "AfterRecordPayloadLogged",
            Self::AfterVersionMetadataChanged => "AfterVersionMetadataChanged",
            Self::AfterPrimaryIndexModified => "AfterPrimaryIndexModified",
            Self::AfterSecondaryIndexModified => "AfterSecondaryIndexModified",
            Self::DuringOverflowPageAlloc => "DuringOverflowPageAlloc",
            Self::DuringRecordSlotReclaim => "DuringRecordSlotReclaim",
            Self::DuringFreelistUpdate => "DuringFreelistUpdate",
            Self::BeforeCommitRecordLogged => "BeforeCommitRecordLogged",
            Self::AfterCommitRecordLoggedBeforeFlush => "AfterCommitRecordLoggedBeforeFlush",
            Self::AfterDurableCommitRecordFlush => "AfterDurableCommitRecordFlush",
            Self::DuringHeaderPersistence => "DuringHeaderPersistence",
            Self::DuringVacuumIndexCleanup => "DuringVacuumIndexCleanup",
            Self::DuringVacuumRecordReclaim => "DuringVacuumRecordReclaim",
            Self::DuringVacuumFreelistUpdate => "DuringVacuumFreelistUpdate",
        }
    }
}
