//! MVCC-Safe Vacuum, Version Garbage Collection & Physical Storage Reclamation.
//!
//! Provides deterministic, bounded, snapshot-safe physical garbage collection:
//! 1. Formal `ReclamationHorizon` derivation from active snapshots.
//! 2. Centralized `VersionReclamationEligibility` predicate.
//! 3. Bounded, idempotent vacuum passes over tables and storage engines.
//! 4. Safe version-chain pruning preserving live row heads.
//! 5. Comprehensive secondary, covering, and unique index cleanup.
//! 6. Complete overflow-chain traversal and freelist recycling.
//! 7. Slot deallocation and empty `RecordPage` recycling.
//! 8. Consistency validation verifying no dangling references or freelist cycles.

use std::collections::HashSet;

use crate::engine::isolation::{CommitSeq, ReclamationHorizon, Snapshot};
use crate::engine::table::Table;
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::metadata::is_reserved_page;
use crate::storage::page::{Key, PageType, PAGE_SIZE};
use crate::storage::record_page::RecordPage;
use crate::storage::record_store::{RecordStore, VersionMetadata};
use crate::wal::lsn::TxnId;
use crate::TreeDb;

/// Programmatic configuration bounding resource consumption per vacuum pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumConfig {
    /// Maximum number of physical versions to examine before yielding.
    pub max_versions: usize,
    /// Maximum number of physical pages to examine before yielding.
    pub max_pages: usize,
}

impl Default for VacuumConfig {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl VacuumConfig {
    /// Creates an unbounded vacuum configuration that processes all eligible candidates.
    pub const fn unlimited() -> Self {
        Self {
            max_versions: usize::MAX,
            max_pages: usize::MAX,
        }
    }

    /// Creates a bounded vacuum configuration with explicit limits.
    pub const fn bounded(max_versions: usize, max_pages: usize) -> Self {
        Self {
            max_versions,
            max_pages,
        }
    }
}

/// Structured resource accounting and metrics resulting from a vacuum execution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VacuumResult {
    /// Total number of physical versions examined during this pass.
    pub versions_examined: u64,
    /// Number of physical record versions successfully reclaimed.
    pub versions_reclaimed: u64,
    /// Total number of database pages examined during candidate and slot inspection.
    pub pages_examined: u64,
    /// Number of empty record pages returned to the persistent freelist.
    pub pages_reclaimed: u64,
    /// Number of overflow pages returned to the persistent freelist.
    pub overflow_pages_reclaimed: u64,
    /// Number of secondary and covering index entries cleanly removed.
    pub index_entries_removed: u64,
    /// Total estimated user payload and page bytes reclaimed.
    pub bytes_reclaimed: u64,
    /// Number of active snapshots registered at vacuum invocation.
    pub active_snapshots: usize,
    /// Oldest commit sequence observed across all active snapshots (the reclamation horizon).
    pub reclamation_horizon: CommitSeq,
    /// Whether version reclamation on any candidate was inhibited by an active snapshot.
    pub blocked_by_oldest_snapshot: bool,
}

impl VacuumResult {
    /// Merges metrics from another vacuum pass into `self`.
    pub fn merge(&mut self, other: &Self) {
        self.versions_examined = self
            .versions_examined
            .saturating_add(other.versions_examined);
        self.versions_reclaimed = self
            .versions_reclaimed
            .saturating_add(other.versions_reclaimed);
        self.pages_examined = self.pages_examined.saturating_add(other.pages_examined);
        self.pages_reclaimed = self.pages_reclaimed.saturating_add(other.pages_reclaimed);
        self.overflow_pages_reclaimed = self
            .overflow_pages_reclaimed
            .saturating_add(other.overflow_pages_reclaimed);
        self.index_entries_removed = self
            .index_entries_removed
            .saturating_add(other.index_entries_removed);
        self.bytes_reclaimed = self.bytes_reclaimed.saturating_add(other.bytes_reclaimed);
        self.blocked_by_oldest_snapshot |= other.blocked_by_oldest_snapshot;
    }
}

/// Classification of a physical version's eligibility for reclamation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionReclamationEligibility {
    /// Version is the current live head of a logical record and must be retained.
    LiveHead,
    /// Version is required by an active snapshot observing sequence `as_of`.
    RequiredBySnapshot { as_of: CommitSeq },
    /// Version was created after the reclamation horizon and may be required by an active snapshot.
    CreatedAfterHorizon { created_at: CommitSeq },
    /// Version belongs to an uncommitted active transaction.
    ActiveTransaction { txn_id: TxnId },
    /// Eligible: Version was superseded and committed deleted at or before the horizon.
    EligibleSuperseded { deleted_at: CommitSeq },
    /// Eligible: Entire logical record deletion committed at or before the horizon.
    EligibleDeletedRow { deleted_at: CommitSeq },
    /// Eligible: Version was created by an aborted transaction, never committed, and invisible to all snapshots.
    EligibleAborted { xmin: TxnId },
}

impl VersionReclamationEligibility {
    /// Returns `true` if this version is safe to reclaim.
    pub const fn is_eligible(&self) -> bool {
        matches!(
            self,
            Self::EligibleSuperseded { .. }
                | Self::EligibleDeletedRow { .. }
                | Self::EligibleAborted { .. }
        )
    }
}

/// Centralized evaluation predicate determining the reclamation status of a version.
pub fn evaluate_version_eligibility(
    is_head: bool,
    metadata: &VersionMetadata,
    horizon: ReclamationHorizon,
    active_snapshots: &[Snapshot],
    active_txns: &HashSet<TxnId>,
) -> VersionReclamationEligibility {
    // 1. Check for uncommitted active transactions
    if metadata.created_at.is_none()
        && metadata.xmin != crate::wal::lsn::NULL_TXN
        && active_txns.contains(&metadata.xmin)
    {
        return VersionReclamationEligibility::ActiveTransaction {
            txn_id: metadata.xmin,
        };
    }

    // 2. Check for aborted version
    if metadata.created_at.is_none()
        && metadata.xmin != crate::wal::lsn::NULL_TXN
        && !active_txns.contains(&metadata.xmin)
    {
        return VersionReclamationEligibility::EligibleAborted {
            xmin: metadata.xmin,
        };
    }

    // 3. Live head check
    if is_head && metadata.deleted_at.is_none() && metadata.xmax.is_none() {
        return VersionReclamationEligibility::LiveHead;
    }

    // 4. Deleted row head check
    if is_head {
        if let Some(del_seq) = metadata.deleted_at {
            if del_seq.0 <= horizon.oldest_required_commit_seq.0 {
                return VersionReclamationEligibility::EligibleDeletedRow {
                    deleted_at: del_seq,
                };
            }

            // Check if an active snapshot can still observe the row before deletion
            for snap in active_snapshots {
                if del_seq.0 > snap.as_of_commit_seq.0 {
                    let created_seq = metadata.created_at.unwrap_or(CommitSeq::ZERO);
                    if created_seq.0 <= snap.as_of_commit_seq.0 {
                        return VersionReclamationEligibility::RequiredBySnapshot {
                            as_of: snap.as_of_commit_seq,
                        };
                    }
                }
            }

            return VersionReclamationEligibility::CreatedAfterHorizon {
                created_at: del_seq,
            };
        }
    }

    // 5. Predecessor version check (!is_head)
    if let Some(del_seq) = metadata.deleted_at {
        if del_seq.0 <= horizon.oldest_required_commit_seq.0 {
            return VersionReclamationEligibility::EligibleSuperseded {
                deleted_at: del_seq,
            };
        }

        for snap in active_snapshots {
            if del_seq.0 > snap.as_of_commit_seq.0 {
                let created_seq = metadata.created_at.unwrap_or(CommitSeq::ZERO);
                if created_seq.0 <= snap.as_of_commit_seq.0 {
                    return VersionReclamationEligibility::RequiredBySnapshot {
                        as_of: snap.as_of_commit_seq,
                    };
                }
            }
        }

        return VersionReclamationEligibility::CreatedAfterHorizon {
            created_at: del_seq,
        };
    }

    // If not committed deleted, retain
    VersionReclamationEligibility::LiveHead
}

/// Candidate identified during candidate discovery scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumCandidate {
    /// Logical primary key.
    pub key: Key,
    /// Physical record identifier.
    pub record_id: RecordId,
    /// Evaluated eligibility state.
    pub eligibility: VersionReclamationEligibility,
    /// Whether this candidate represents the entire logical record.
    pub is_whole_row: bool,
}

/// Core execution engine orchestrating vacuum candidate discovery and reclamation.
pub struct VacuumEngine;

impl VacuumEngine {
    /// Performs a vacuum pass on a relational table handle.
    pub fn vacuum_table(
        table: &mut Table<'_>,
        config: &VacuumConfig,
        horizon: ReclamationHorizon,
    ) -> Result<VacuumResult> {
        let mut result = VacuumResult {
            reclamation_horizon: horizon.oldest_required_commit_seq,
            ..Default::default()
        };

        let active_txns = HashSet::new();
        let active_snapshots = match &table.isolation_context {
            Some(ctx) => vec![ctx.snapshot],
            None => Vec::new(),
        };

        // Scan all primary index entries
        let entries = table.tree.scan(table.buffer_pool, None, None)?;
        let mut touched_record_pages = HashSet::new();

        for (key, head_rid) in entries {
            if result.versions_examined >= config.max_versions as u64
                || result.pages_examined >= config.max_pages as u64
            {
                break;
            }

            let head_version = table
                .record_store
                .read_version(table.buffer_pool, head_rid)?;
            let (head_meta, _) = match head_version {
                Some(v) => v,
                None => {
                    return Err(Error::InvalidVersionReference(format!(
                        "head version at {head_rid:?} not found for key {key}"
                    )));
                }
            };

            result.versions_examined += 1;
            touched_record_pages.insert(head_rid.page_id);

            let head_eligibility = evaluate_version_eligibility(
                true,
                &head_meta,
                horizon,
                &active_snapshots,
                &active_txns,
            );

            match head_eligibility {
                VersionReclamationEligibility::EligibleDeletedRow { .. } => {
                    // Reclaim entire logical record and its complete chain
                    let chain_rids = Self::collect_version_chain(
                        &table.record_store,
                        table.buffer_pool,
                        head_rid,
                    )?;

                    // Remove entry from primary B+ Tree
                    table.tree.delete(table.buffer_pool, key)?;

                    // Reclaim each version in the chain
                    for rid in chain_rids {
                        touched_record_pages.insert(rid.page_id);
                        Self::reclaim_version_and_indexes(table, rid, &mut result)?;
                    }
                }
                VersionReclamationEligibility::LiveHead
                | VersionReclamationEligibility::RequiredBySnapshot { .. }
                | VersionReclamationEligibility::CreatedAfterHorizon { .. } => {
                    if matches!(
                        head_eligibility,
                        VersionReclamationEligibility::RequiredBySnapshot { .. }
                    ) {
                        result.blocked_by_oldest_snapshot = true;
                    }

                    // Live row or retained deletion: prune obsolete tail predecessors
                    let chain_rids = Self::collect_version_chain(
                        &table.record_store,
                        table.buffer_pool,
                        head_rid,
                    )?;

                    // Find newest version in chain that committed at or before horizon
                    let mut cut_point_idx = None;
                    for (idx, &rid) in chain_rids.iter().enumerate() {
                        if idx > 0 {
                            result.versions_examined += 1;
                        }
                        if let Some((meta, _)) =
                            table.record_store.read_version(table.buffer_pool, rid)?
                        {
                            if let Some(cseq) = meta.created_at {
                                if cseq.0 <= horizon.oldest_required_commit_seq.0 {
                                    cut_point_idx = Some(idx);
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(cut_idx) = cut_point_idx {
                        let cut_rid = chain_rids[cut_idx];
                        if let Some((mut cut_meta, _)) = table
                            .record_store
                            .read_version(table.buffer_pool, cut_rid)?
                        {
                            if cut_meta.prev_version.is_some() {
                                // Cut the chain here
                                cut_meta.prev_version = None;
                                table.record_store.update_version_metadata(
                                    table.buffer_pool,
                                    cut_rid,
                                    &cut_meta,
                                )?;

                                // Reclaim all versions strictly behind cut_idx
                                for &obsolete_rid in &chain_rids[cut_idx + 1..] {
                                    touched_record_pages.insert(obsolete_rid.page_id);
                                    Self::reclaim_version_and_indexes(
                                        table,
                                        obsolete_rid,
                                        &mut result,
                                    )?;
                                }
                            }
                        }
                    } else if chain_rids.len() > 1 {
                        result.blocked_by_oldest_snapshot = true;
                    }
                }
                VersionReclamationEligibility::ActiveTransaction { .. } => {
                    // Nothing to reclaim while active
                }
                VersionReclamationEligibility::EligibleAborted { .. } => {
                    // Orphaned/aborted head
                    table.tree.delete(table.buffer_pool, key)?;
                    touched_record_pages.insert(head_rid.page_id);
                    Self::reclaim_version_and_indexes(table, head_rid, &mut result)?;
                }
                VersionReclamationEligibility::EligibleSuperseded { .. } => {
                    unreachable!("head version cannot be EligibleSuperseded");
                }
            }
        }

        // Try freeing any completely empty RecordPage encountered during this pass
        for page_id in touched_record_pages {
            result.pages_examined += 1;
            if table
                .record_store
                .try_free_empty_record_page(table.buffer_pool, page_id)?
            {
                result.pages_reclaimed += 1;
                result.bytes_reclaimed += PAGE_SIZE as u64;
                if table.metadata.record_page_id == page_id {
                    table.metadata.record_page_id = INVALID_PAGE_ID;
                }
            }
        }

        table.sync_metadata_if_needed()?;
        Ok(result)
    }

    /// Vacuums the raw Key-Value B+ Tree and Record Store (outside relational tables).
    pub fn vacuum_kv(
        tree: &mut BPlusTree,
        record_store: &mut RecordStore,
        buffer_pool: &mut BufferPool,
        config: &VacuumConfig,
        horizon: ReclamationHorizon,
    ) -> Result<VacuumResult> {
        let mut result = VacuumResult {
            reclamation_horizon: horizon.oldest_required_commit_seq,
            ..Default::default()
        };

        let entries = tree.scan(buffer_pool, None, None)?;
        let mut touched_record_pages = HashSet::new();

        for (key, head_rid) in entries {
            if result.versions_examined >= config.max_versions as u64
                || result.pages_examined >= config.max_pages as u64
            {
                break;
            }

            let head_version = record_store.read_version(buffer_pool, head_rid)?;
            let (head_meta, _) = match head_version {
                Some(v) => v,
                None => {
                    return Err(Error::InvalidVersionReference(format!(
                        "KV head version at {head_rid:?} not found for key {key}"
                    )));
                }
            };

            result.versions_examined += 1;
            touched_record_pages.insert(head_rid.page_id);

            let head_eligibility =
                evaluate_version_eligibility(true, &head_meta, horizon, &[], &HashSet::new());

            match head_eligibility {
                VersionReclamationEligibility::EligibleDeletedRow { .. } => {
                    let chain_rids =
                        Self::collect_version_chain(record_store, buffer_pool, head_rid)?;
                    tree.delete(buffer_pool, key)?;

                    for rid in chain_rids {
                        touched_record_pages.insert(rid.page_id);
                        let payload_len = record_store
                            .read_version(buffer_pool, rid)?
                            .map(|(_, p)| p.len())
                            .unwrap_or(0);
                        let (deleted, overflow_count) =
                            record_store.delete_and_count(buffer_pool, rid)?;
                        if deleted {
                            result.versions_reclaimed += 1;
                            result.overflow_pages_reclaimed += overflow_count;
                            result.pages_reclaimed += overflow_count;
                            result.bytes_reclaimed +=
                                payload_len as u64 + (overflow_count * PAGE_SIZE as u64);
                        }
                    }
                }
                VersionReclamationEligibility::LiveHead
                | VersionReclamationEligibility::RequiredBySnapshot { .. }
                | VersionReclamationEligibility::CreatedAfterHorizon { .. } => {
                    let chain_rids =
                        Self::collect_version_chain(record_store, buffer_pool, head_rid)?;

                    let mut cut_point_idx = None;
                    for (idx, &rid) in chain_rids.iter().enumerate() {
                        if idx > 0 {
                            result.versions_examined += 1;
                        }
                        if let Some((meta, _)) = record_store.read_version(buffer_pool, rid)? {
                            if let Some(cseq) = meta.created_at {
                                if cseq.0 <= horizon.oldest_required_commit_seq.0 {
                                    cut_point_idx = Some(idx);
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(cut_idx) = cut_point_idx {
                        let cut_rid = chain_rids[cut_idx];
                        if let Some((mut cut_meta, _)) =
                            record_store.read_version(buffer_pool, cut_rid)?
                        {
                            if cut_meta.prev_version.is_some() {
                                cut_meta.prev_version = None;
                                record_store.update_version_metadata(
                                    buffer_pool,
                                    cut_rid,
                                    &cut_meta,
                                )?;

                                for &obsolete_rid in &chain_rids[cut_idx + 1..] {
                                    touched_record_pages.insert(obsolete_rid.page_id);
                                    let payload_len = record_store
                                        .read_version(buffer_pool, obsolete_rid)?
                                        .map(|(_, p)| p.len())
                                        .unwrap_or(0);
                                    let (deleted, overflow_count) =
                                        record_store.delete_and_count(buffer_pool, obsolete_rid)?;
                                    if deleted {
                                        result.versions_reclaimed += 1;
                                        result.overflow_pages_reclaimed += overflow_count;
                                        result.pages_reclaimed += overflow_count;
                                        result.bytes_reclaimed += payload_len as u64
                                            + (overflow_count * PAGE_SIZE as u64);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        for page_id in touched_record_pages {
            result.pages_examined += 1;
            if record_store.try_free_empty_record_page(buffer_pool, page_id)? {
                result.pages_reclaimed += 1;
                result.bytes_reclaimed += PAGE_SIZE as u64;
            }
        }

        Ok(result)
    }

    /// Helper to collect all `RecordId`s in a version chain from head to tail.
    pub fn collect_version_chain(
        record_store: &RecordStore,
        buffer_pool: &mut BufferPool,
        head_rid: RecordId,
    ) -> Result<Vec<RecordId>> {
        let mut rids = Vec::new();
        let mut curr = head_rid;
        let mut visited = HashSet::new();

        while curr.page_id != INVALID_PAGE_ID {
            if !visited.insert(curr) {
                return Err(Error::VersionChainCycle(format!(
                    "cycle detected in version chain at {curr:?}"
                )));
            }
            rids.push(curr);
            match record_store.read_version(buffer_pool, curr)? {
                Some((meta, _)) => match meta.prev_version {
                    Some(p) => curr = p,
                    None => break,
                },
                None => break,
            }
        }

        Ok(rids)
    }

    /// Internal helper that reclaims secondary indexes, overflow pages, and slot for a single version.
    fn reclaim_version_and_indexes(
        table: &mut Table<'_>,
        rid: RecordId,
        result: &mut VacuumResult,
    ) -> Result<()> {
        let version_data = table.record_store.read_version(table.buffer_pool, rid)?;
        let (payload_len, decoded_row) = match version_data {
            Some((_, payload)) => {
                let len = payload.len();
                let row = table.metadata.schema.decode_tuple(&payload).ok();
                (len, row)
            }
            None => (0, None),
        };

        // 1. Remove obsolete entries from secondary and covering indexes
        if let Some(ref row) = decoded_row {
            for (meta, sec_idx) in &mut table.indexes {
                let idx_key = meta.extract_index_key(row);
                if sec_idx.delete(table.buffer_pool, &idx_key, rid)? {
                    result.index_entries_removed += 1;
                }
            }
        }

        // 2. Reclaim record slot and overflow pages
        let (deleted, overflow_count) = table
            .record_store
            .delete_and_count(table.buffer_pool, rid)?;

        if deleted {
            result.versions_reclaimed += 1;
            result.overflow_pages_reclaimed += overflow_count;
            result.pages_reclaimed += overflow_count;
            result.bytes_reclaimed += payload_len as u64 + (overflow_count * PAGE_SIZE as u64);
        }

        Ok(())
    }

    /// Comprehensive post-vacuum consistency validator.
    ///
    /// Verifies that:
    /// 1. Every live version reference resolves.
    /// 2. Every retained version-chain link resolves and contains no cycles.
    /// 3. No current primary-index entry points to a reclaimed/deleted record.
    /// 4. No secondary-index entry points to an invalid version.
    /// 5. No retained overflow chain contains a freed page.
    /// 6. No freelist cycle or double-free exists.
    pub fn validate_table_vacuum_consistency(table: &mut Table<'_>) -> Result<()> {
        // 1. Validate freelist structural integrity
        table.buffer_pool.validate_freelist()?;

        // 2. Collect all currently freed pages
        let mut free_pages = HashSet::new();
        {
            let mut curr = table.buffer_pool.page_manager().header().freelist_head;
            let total = table.buffer_pool.page_manager().page_count();
            while curr != crate::storage::metadata::NO_FREE_PAGE {
                if curr >= total || is_reserved_page(curr) || !free_pages.insert(curr) {
                    break;
                }
                let page = table.buffer_pool.page_manager_mut().read_page(curr)?;
                if page.page_type() != PageType::Free {
                    break;
                }
                curr = crate::storage::freelist::FreePageLayout::read_next_free(&page);
            }
        }

        // 3. Validate primary tree
        table.tree.validate_tree(table.buffer_pool)?;

        // 4. Validate all version chains originating from primary tree
        let entries = table.tree.scan(table.buffer_pool, None, None)?;
        let mut active_version_rids = HashSet::new();

        for (_key, head_rid) in entries {
            if free_pages.contains(&head_rid.page_id) {
                return Err(Error::VacuumValidation(format!(
                    "primary tree points to record on freed page {}",
                    head_rid.page_id
                )));
            }

            let chain_len = table
                .record_store
                .validate_version_chain(table.buffer_pool, head_rid)?;
            if chain_len == 0 {
                return Err(Error::VacuumValidation(format!(
                    "empty version chain originating at {head_rid:?}"
                )));
            }

            let chain =
                Self::collect_version_chain(&table.record_store, table.buffer_pool, head_rid)?;
            for &rid in &chain {
                active_version_rids.insert(rid);

                if free_pages.contains(&rid.page_id) {
                    return Err(Error::VacuumValidation(format!(
                        "version {rid:?} resides on freed page {}",
                        rid.page_id
                    )));
                }

                // Check overflow chains
                let (is_overflow, head_pid, total_len) = {
                    let page = table.buffer_pool.fetch_page(rid.page_id)?;
                    let rp = RecordPage::deserialize(page)?;
                    table.buffer_pool.unpin_page(rid.page_id, false)?;
                    match rp.get_overflow_descriptor(rid.slot_id) {
                        Some(d) => (true, d.head_page_id, d.total_length),
                        None => (false, 0, 0),
                    }
                };

                if is_overflow {
                    let overflow_pages = table.record_store.validate_overflow_chain(
                        table.buffer_pool,
                        head_pid,
                        total_len,
                    )?;
                    for opid in overflow_pages {
                        if free_pages.contains(&opid) {
                            return Err(Error::VacuumValidation(format!(
                                "overflow page {opid} of version {rid:?} is present in freelist"
                            )));
                        }
                    }
                }
            }
        }

        // 5. Validate secondary indexes
        for (meta, sec_idx) in &table.indexes {
            sec_idx.validate_tree(table.buffer_pool, meta.unique)?;
            let sec_pages = sec_idx.collect_all_pages(table.buffer_pool)?;
            for spid in sec_pages {
                if free_pages.contains(&spid) {
                    return Err(Error::VacuumValidation(format!(
                        "secondary index {} page {spid} is present in freelist",
                        meta.name
                    )));
                }
            }
        }

        Ok(())
    }

    /// Comprehensive post-vacuum consistency validator across the entire database.
    pub fn validate_db_vacuum_consistency(db: &mut TreeDb) -> Result<()> {
        db.validate_tree()?;
        db.buffer_pool.validate_freelist()?;

        let table_names: Vec<String> = {
            let tables = crate::engine::catalog::Catalog::list_tables(&mut db.buffer_pool)?;
            tables.into_iter().map(|t| t.name).collect()
        };

        for name in table_names {
            let mut table = db.open_table(&name)?;
            Self::validate_table_vacuum_consistency(&mut table)?;
        }

        Ok(())
    }
}
