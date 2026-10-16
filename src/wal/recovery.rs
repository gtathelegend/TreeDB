use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::page::{Page, PageId};
use crate::storage::page_manager::PageManager;
use crate::wal::log_manager::{LogManager, WAL_HEADER_SIZE};
use crate::wal::log_record::{LogRecordBody, LogRecordType};
use crate::wal::lsn::{Lsn, TxnId};

/// Status of a transaction during crash recovery analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    Active,
    Committed,
    Aborted,
}

/// Transaction state tracked during the analysis pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnTableEntry {
    pub status: TxnStatus,
    pub last_lsn: Lsn,
    pub undo_next_lsn: Lsn,
}

/// Summary metrics returned upon completing crash recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryStats {
    pub records_scanned: usize,
    pub active_txns: usize,
    pub committed_txns: usize,
    pub loser_txns: usize,
    pub redo_count: usize,
    pub undo_count: usize,
    pub clrs_generated: usize,
    pub checkpoint_lsn: Lsn,
    pub max_commit_seq: u64,
}

/// Simulated crash / fault injection point during crash recovery testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryFault {
    /// Crash before starting analysis.
    BeforeAnalysis,
    /// Crash after analysis but before redo.
    BeforeRedo,
    /// Crash during redo after replaying `N` records.
    DuringRedo(usize),
    /// Crash after redo but before undo.
    BeforeUndo,
    /// Crash immediately after appending CLR but before flushing WAL.
    AfterClrAppendBeforeFlush,
    /// Crash immediately after flushing CLR but before writing recovered page.
    AfterClrFlushBeforePageWrite,
    /// Crash immediately after writing recovered page for `N`-th undo.
    AfterRecoveredPageWrite(usize),
}

/// ARIES-style Crash Recovery Engine executing Analysis, Redo, and Undo passes.
pub struct RecoveryEngine;

impl RecoveryEngine {
    /// Executes full ARIES recovery on the given `PageManager` using the provided `LogManager`.
    pub fn recover(
        page_manager: &mut PageManager,
        log_manager: &mut LogManager,
    ) -> Result<RecoveryStats> {
        Self::recover_with_fault(page_manager, log_manager, None)
    }

    /// Executes ARIES recovery with deterministic fault injection for crash simulation.
    pub fn recover_with_fault(
        page_manager: &mut PageManager,
        log_manager: &mut LogManager,
        fault: Option<RecoveryFault>,
    ) -> Result<RecoveryStats> {
        if fault == Some(RecoveryFault::BeforeAnalysis) {
            return Err(Error::RecoveryInterrupted(
                "simulated crash before analysis".into(),
            ));
        }

        let mut stats = RecoveryStats::default();

        let last_checkpoint_lsn = page_manager.header().last_checkpoint_lsn;
        let mut txn_table: HashMap<TxnId, TxnTableEntry> = HashMap::new();
        let mut dpt: HashMap<PageId, Lsn> = HashMap::new();
        let mut dpt_first_offset: HashMap<PageId, u64> = HashMap::new();
        let mut loser_record_offsets: HashMap<Lsn, u64> = HashMap::new();
        let mut active_txn_lsns: HashMap<TxnId, Vec<Lsn>> = HashMap::new();

        // Validate checkpoint LSN bounds against durable flushed LSN
        if last_checkpoint_lsn.is_valid()
            && log_manager.flushed_lsn().is_valid()
            && last_checkpoint_lsn > log_manager.flushed_lsn()
        {
            return Err(Error::WalCorruption(format!(
                "checkpoint LSN {} exceeds durable flushed LSN {}",
                last_checkpoint_lsn,
                log_manager.flushed_lsn()
            )));
        }

        // -------------------------------------------------------------------
        // 0. INITIAL PROBE: Record count & Checkpoint location
        // -------------------------------------------------------------------
        let mut total_records_in_wal = 0;
        let mut checkpoint_record = None;
        let mut cp_active_txns = Vec::new();
        let mut start_offset = WAL_HEADER_SIZE as u64;

        {
            let mut reader = log_manager.reader()?;
            while let Some((rec, offset, _)) = reader.next_record_with_offset()? {
                total_records_in_wal += 1;
                if last_checkpoint_lsn.is_valid() && rec.lsn == last_checkpoint_lsn {
                    if rec.record_type != LogRecordType::Checkpoint {
                        return Err(Error::WalCorruption(format!(
                            "record at checkpoint LSN {} is not a Checkpoint record (type={:?})",
                            last_checkpoint_lsn, rec.record_type
                        )));
                    }
                    if let LogRecordBody::Checkpoint { active_txns } = &rec.body {
                        for &(txn_id, last_lsn) in active_txns {
                            if !txn_id.is_valid()
                                || !last_lsn.is_valid()
                                || last_lsn >= last_checkpoint_lsn
                            {
                                return Err(Error::WalCorruption(format!(
                                    "corrupted active transaction entry (txn_id={txn_id}, last_lsn={last_lsn}) in checkpoint record {last_checkpoint_lsn}"
                                )));
                            }
                        }
                        cp_active_txns = active_txns.clone();
                    }
                    checkpoint_record = Some(offset);
                }
            }
        }

        stats.records_scanned = total_records_in_wal;
        if total_records_in_wal == 0 {
            return Ok(stats);
        }

        if last_checkpoint_lsn.is_valid() {
            let cp_offset = checkpoint_record.ok_or_else(|| {
                Error::WalCorruption(format!(
                    "checkpoint LSN {} referenced in header not found in WAL",
                    last_checkpoint_lsn
                ))
            })?;

            stats.checkpoint_lsn = last_checkpoint_lsn;

            for &(txn_id, last_lsn) in &cp_active_txns {
                txn_table.insert(
                    txn_id,
                    TxnTableEntry {
                        status: TxnStatus::Active,
                        last_lsn,
                        undo_next_lsn: last_lsn,
                    },
                );
            }

            let min_active_lsn = txn_table.values().map(|e| e.last_lsn).min();
            let start_lsn = match min_active_lsn {
                Some(min_l) => std::cmp::min(min_l, last_checkpoint_lsn),
                None => last_checkpoint_lsn,
            };

            if start_lsn == last_checkpoint_lsn {
                start_offset = cp_offset;
            } else {
                let mut reader = log_manager.reader()?;
                while let Some((rec, offset, _)) = reader.next_record_with_offset()? {
                    if rec.lsn >= start_lsn {
                        start_offset = offset;
                        break;
                    }
                }
            }
        }

        // -------------------------------------------------------------------
        // 1. ANALYSIS PASS (Incremental Streaming)
        // -------------------------------------------------------------------
        let mut reader = log_manager.reader()?;
        reader.seek_to_offset(start_offset)?;

        while let Some((rec, rec_offset, _)) = reader.next_record_with_offset()? {
            match rec.record_type {
                LogRecordType::Begin => {
                    if !txn_table.contains_key(&rec.txn_id) {
                        stats.active_txns += 1;
                    }
                    txn_table.insert(
                        rec.txn_id,
                        TxnTableEntry {
                            status: TxnStatus::Active,
                            last_lsn: rec.lsn,
                            undo_next_lsn: rec.lsn,
                        },
                    );
                    active_txn_lsns.entry(rec.txn_id).or_default().push(rec.lsn);
                    loser_record_offsets.insert(rec.lsn, rec_offset);
                }
                LogRecordType::Commit => {
                    stats.committed_txns += 1;
                    txn_table.remove(&rec.txn_id);
                    if let LogRecordBody::Commit { commit_seq } = rec.body {
                        stats.max_commit_seq = std::cmp::max(stats.max_commit_seq, commit_seq);
                    }
                    if let Some(lsns) = active_txn_lsns.remove(&rec.txn_id) {
                        for l in lsns {
                            loser_record_offsets.remove(&l);
                        }
                    }
                }
                LogRecordType::Abort => {
                    txn_table.remove(&rec.txn_id);
                    if let Some(lsns) = active_txn_lsns.remove(&rec.txn_id) {
                        for l in lsns {
                            loser_record_offsets.remove(&l);
                        }
                    }
                }
                LogRecordType::PageDelta => {
                    if !txn_table.contains_key(&rec.txn_id) {
                        stats.active_txns += 1;
                    }
                    let entry = txn_table.entry(rec.txn_id).or_insert(TxnTableEntry {
                        status: TxnStatus::Active,
                        last_lsn: rec.lsn,
                        undo_next_lsn: rec.lsn,
                    });
                    entry.last_lsn = rec.lsn;
                    entry.undo_next_lsn = rec.lsn;

                    active_txn_lsns.entry(rec.txn_id).or_default().push(rec.lsn);
                    loser_record_offsets.insert(rec.lsn, rec_offset);

                    if let LogRecordBody::PageDelta { page_id, .. } = rec.body {
                        dpt.entry(page_id).or_insert(rec.lsn);
                        dpt_first_offset.entry(page_id).or_insert(rec_offset);
                    }
                }
                LogRecordType::Clr => {
                    if let LogRecordBody::Clr {
                        page_id,
                        undo_next_lsn,
                        ..
                    } = &rec.body
                    {
                        if !txn_table.contains_key(&rec.txn_id) {
                            stats.active_txns += 1;
                        }
                        let entry = txn_table.entry(rec.txn_id).or_insert(TxnTableEntry {
                            status: TxnStatus::Active,
                            last_lsn: rec.lsn,
                            undo_next_lsn: *undo_next_lsn,
                        });
                        entry.last_lsn = rec.lsn;
                        entry.undo_next_lsn = *undo_next_lsn;

                        active_txn_lsns.entry(rec.txn_id).or_default().push(rec.lsn);
                        loser_record_offsets.insert(rec.lsn, rec_offset);

                        dpt.entry(*page_id).or_insert(rec.lsn);
                        dpt_first_offset.entry(*page_id).or_insert(rec_offset);
                    }
                }
                LogRecordType::Checkpoint => {
                    if let LogRecordBody::Checkpoint { active_txns } = &rec.body {
                        for &(txn_id, last_lsn) in active_txns {
                            if !txn_table.contains_key(&txn_id) {
                                stats.active_txns += 1;
                            }
                            txn_table.entry(txn_id).or_insert(TxnTableEntry {
                                status: TxnStatus::Active,
                                last_lsn,
                                undo_next_lsn: last_lsn,
                            });
                        }
                    }
                }
            }
        }

        stats.loser_txns = txn_table.len();

        if fault == Some(RecoveryFault::BeforeRedo) {
            return Err(Error::RecoveryInterrupted(
                "simulated crash before redo".into(),
            ));
        }

        // -------------------------------------------------------------------
        // 2. REDO PASS (Incremental Streaming)
        // -------------------------------------------------------------------
        let min_rec_lsn = dpt.values().copied().min();
        if let Some(min_rec) = min_rec_lsn {
            let redo_start_offset = dpt
                .iter()
                .filter(|(_, &lsn)| lsn == min_rec)
                .find_map(|(pid, _)| dpt_first_offset.get(pid).copied())
                .unwrap_or(WAL_HEADER_SIZE as u64);

            reader.seek_to_offset(redo_start_offset)?;

            while let Some((rec, _, _)) = reader.next_record_with_offset()? {
                if rec.lsn < min_rec {
                    continue;
                }
                match &rec.body {
                    LogRecordBody::PageDelta {
                        page_id,
                        offset,
                        after_image,
                        ..
                    } if *page_id < page_manager.page_count() => {
                        if let Some(&rec_lsn) = dpt.get(page_id) {
                            if rec.lsn >= rec_lsn {
                                let mut page = page_manager.read_page(*page_id)?;
                                if page.lsn() < rec.lsn {
                                    let start = *offset as usize;
                                    let end = start + after_image.len();
                                    if end <= page.as_bytes().len() {
                                        page.as_bytes_mut()[start..end]
                                            .copy_from_slice(after_image);
                                        page.set_lsn(rec.lsn);
                                        page_manager.write_page(&page)?;
                                        stats.redo_count += 1;

                                        if fault
                                            == Some(RecoveryFault::DuringRedo(stats.redo_count))
                                        {
                                            return Err(Error::RecoveryInterrupted(
                                                "simulated crash during redo".into(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    LogRecordBody::Clr {
                        page_id,
                        offset,
                        redo_image,
                        ..
                    } if *page_id < page_manager.page_count() => {
                        if let Some(&rec_lsn) = dpt.get(page_id) {
                            if rec.lsn >= rec_lsn {
                                let mut page = page_manager.read_page(*page_id)?;
                                if page.lsn() < rec.lsn {
                                    let start = *offset as usize;
                                    let end = start + redo_image.len();
                                    if end <= page.as_bytes().len() {
                                        page.as_bytes_mut()[start..end].copy_from_slice(redo_image);
                                        page.set_lsn(rec.lsn);
                                        page_manager.write_page(&page)?;
                                        stats.redo_count += 1;

                                        if fault
                                            == Some(RecoveryFault::DuringRedo(stats.redo_count))
                                        {
                                            return Err(Error::RecoveryInterrupted(
                                                "simulated crash during redo".into(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if fault == Some(RecoveryFault::BeforeUndo) {
            return Err(Error::RecoveryInterrupted(
                "simulated crash before undo".into(),
            ));
        }

        // -------------------------------------------------------------------
        // 3. UNDO PASS (Targeted Record Reads)
        // -------------------------------------------------------------------
        let mut active_undo_lsns: Vec<(Lsn, TxnId)> = txn_table
            .iter()
            .filter(|(_, entry)| {
                entry.status == TxnStatus::Active && entry.undo_next_lsn.is_valid()
            })
            .map(|(&txn_id, entry)| (entry.undo_next_lsn, txn_id))
            .collect();

        // Drop the reader borrow on log_manager so log_manager can append and flush CLRs
        drop(reader);

        while !active_undo_lsns.is_empty() {
            // Find maximum LSN among active losers to undo in reverse chronological order
            active_undo_lsns.sort_by_key(|&(lsn, _)| lsn);
            let (curr_lsn, txn_id) = active_undo_lsns.pop().unwrap();

            let rec = if let Some(&offset) = loser_record_offsets.get(&curr_lsn) {
                match log_manager.read_record_at_offset(offset) {
                    Ok(r) if r.lsn == curr_lsn && r.txn_id == txn_id => Some(r),
                    _ => log_manager.read_record_for_txn(txn_id, curr_lsn)?,
                }
            } else {
                log_manager.read_record_for_txn(txn_id, curr_lsn)?
            };

            let rec = rec.ok_or_else(|| {
                Error::WalCorruption(format!(
                    "Recovery undo failed: record for txn {txn_id} at LSN {curr_lsn} not found"
                ))
            })?;

            match &rec.body {
                LogRecordBody::PageDelta {
                    page_id,
                    offset,
                    before_image,
                    ..
                } => {
                    let mut page = if *page_id < page_manager.page_count() {
                        page_manager.read_page(*page_id)?
                    } else {
                        Page::new(*page_id, crate::storage::page::PageType::Free)
                    };

                    // Apply before_image (undo change)
                    let start = *offset as usize;
                    let end = start + before_image.len();
                    if end <= page.as_bytes().len() {
                        page.as_bytes_mut()[start..end].copy_from_slice(before_image);
                    }

                    // Append Compensation Log Record (CLR)
                    let clr_lsn = log_manager.append_clr(
                        txn_id,
                        *page_id,
                        *offset,
                        before_image.clone(),
                        rec.prev_lsn,
                    )?;

                    if fault == Some(RecoveryFault::AfterClrAppendBeforeFlush) {
                        return Err(Error::RecoveryInterrupted(
                            "simulated crash after CLR append before flush".into(),
                        ));
                    }

                    // Enforce Write-Ahead ordering for CLRs before updating page on disk
                    log_manager.flush(clr_lsn)?;

                    if fault == Some(RecoveryFault::AfterClrFlushBeforePageWrite) {
                        return Err(Error::RecoveryInterrupted(
                            "simulated crash after CLR flush before page write".into(),
                        ));
                    }

                    page.set_lsn(clr_lsn);
                    page_manager.write_page(&page)?;

                    stats.undo_count += 1;
                    stats.clrs_generated += 1;

                    if fault == Some(RecoveryFault::AfterRecoveredPageWrite(stats.undo_count)) {
                        return Err(Error::RecoveryInterrupted(
                            "simulated crash after recovered page write".into(),
                        ));
                    }

                    if rec.prev_lsn.is_valid() {
                        active_undo_lsns.push((rec.prev_lsn, txn_id));
                    } else {
                        // Transaction undo complete, append Abort record
                        log_manager.abort_txn(txn_id)?;
                        if let Some(entry) = txn_table.get_mut(&txn_id) {
                            entry.status = TxnStatus::Aborted;
                        }
                    }
                }
                LogRecordBody::Clr { undo_next_lsn, .. } => {
                    if undo_next_lsn.is_valid() {
                        active_undo_lsns.push((*undo_next_lsn, txn_id));
                    } else {
                        log_manager.abort_txn(txn_id)?;
                        if let Some(entry) = txn_table.get_mut(&txn_id) {
                            entry.status = TxnStatus::Aborted;
                        }
                    }
                }
                LogRecordBody::Begin => {
                    // Reached beginning of transaction log chain
                    log_manager.abort_txn(txn_id)?;
                    if let Some(entry) = txn_table.get_mut(&txn_id) {
                        entry.status = TxnStatus::Aborted;
                    }
                }
                _ => {}
            }
        }

        // Finalize any active loser transactions that were already completely undone
        // prior to this recovery pass (e.g. crash occurred after final CLR flush but before abort record)
        for (&txn_id, entry) in &mut txn_table {
            if entry.status == TxnStatus::Active && !entry.undo_next_lsn.is_valid() {
                log_manager.abort_txn(txn_id)?;
                entry.status = TxnStatus::Aborted;
            }
        }

        // Flush WAL and page manager to ensure recovered state is persistent
        if log_manager.next_lsn().is_valid() {
            log_manager.flush(log_manager.next_lsn())?;
        }
        page_manager.set_max_commit_seq(stats.max_commit_seq)?;
        page_manager.sync()?;

        Ok(stats)
    }
}
