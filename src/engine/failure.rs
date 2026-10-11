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
    /// Before checkpoint initiation (before flushing dirty pages).
    BeforeCheckpointInitiation,
    /// During checkpoint dirty page flushing.
    DuringCheckpointDirtyPageFlush,
    /// During checkpoint WAL record flushing.
    DuringCheckpointWalFlush,
    /// During checkpoint database header persistence to disk.
    DuringCheckpointHeaderPersistence,
    /// After checkpoint header persistence but before WAL truncation.
    BeforeCheckpointWalTruncation,
    /// During checkpoint WAL prefix truncation.
    DuringCheckpointWalTruncation,
    /// Immediately after checkpoint and WAL truncation completes.
    AfterCheckpointCompleted,
    /// Before starting backup creation or staging directory preparation.
    BeforeBackupCreation,
    /// During database image file copying to backup staging.
    DuringBackupDbCopy,
    /// During WAL file copying to backup staging.
    DuringBackupWalCopy,
    /// During backup manifest writing to disk.
    DuringBackupManifestWrite,
    /// During non-destructive validation of staged backup before publication.
    DuringBackupValidation,
    /// During file and metadata synchronization of backup artifacts.
    DuringBackupSync,
    /// Immediately before final publication (atomic rename) of the backup directory.
    BeforeBackupPublication,
    /// Before validation of the source backup prior to restore.
    BeforeRestoreValidation,
    /// During pre-restore manifest or image validation.
    DuringRestoreValidation,
    /// During database image file copying to restore staging.
    DuringRestoreDbCopy,
    /// During WAL file copying to restore staging.
    DuringRestoreWalCopy,
    /// During backup of existing destination files to temporary staging.
    DuringRestoreDestinationBackup,
    /// During writing or synchronization of the durable restore journal.
    DuringRestoreJournalSync,
    /// Immediately before final publication (atomic replacement) of the restored database.
    BeforeRestorePublication,
    /// During publication (rename) of the primary restored database file.
    DuringRestoreDbPublish,
    /// During publication (rename) of the companion restored WAL file.
    DuringRestoreWalPublish,
    /// During cleanup of obsolete temporary and backup artifacts after publication.
    DuringRestoreCleanup,
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
            Self::BeforeCheckpointInitiation => "BeforeCheckpointInitiation",
            Self::DuringCheckpointDirtyPageFlush => "DuringCheckpointDirtyPageFlush",
            Self::DuringCheckpointWalFlush => "DuringCheckpointWalFlush",
            Self::DuringCheckpointHeaderPersistence => "DuringCheckpointHeaderPersistence",
            Self::BeforeCheckpointWalTruncation => "BeforeCheckpointWalTruncation",
            Self::DuringCheckpointWalTruncation => "DuringCheckpointWalTruncation",
            Self::AfterCheckpointCompleted => "AfterCheckpointCompleted",
            Self::BeforeBackupCreation => "BeforeBackupCreation",
            Self::DuringBackupDbCopy => "DuringBackupDbCopy",
            Self::DuringBackupWalCopy => "DuringBackupWalCopy",
            Self::DuringBackupManifestWrite => "DuringBackupManifestWrite",
            Self::DuringBackupValidation => "DuringBackupValidation",
            Self::DuringBackupSync => "DuringBackupSync",
            Self::BeforeBackupPublication => "BeforeBackupPublication",
            Self::BeforeRestoreValidation => "BeforeRestoreValidation",
            Self::DuringRestoreValidation => "DuringRestoreValidation",
            Self::DuringRestoreDbCopy => "DuringRestoreDbCopy",
            Self::DuringRestoreWalCopy => "DuringRestoreWalCopy",
            Self::DuringRestoreDestinationBackup => "DuringRestoreDestinationBackup",
            Self::DuringRestoreJournalSync => "DuringRestoreJournalSync",
            Self::BeforeRestorePublication => "BeforeRestorePublication",
            Self::DuringRestoreDbPublish => "DuringRestoreDbPublish",
            Self::DuringRestoreWalPublish => "DuringRestoreWalPublish",
            Self::DuringRestoreCleanup => "DuringRestoreCleanup",
        }
    }
}
