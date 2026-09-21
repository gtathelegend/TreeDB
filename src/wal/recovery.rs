use std::collections::HashMap;

use crate::error::Result;
use crate::storage::page::Page;
use crate::storage::page_manager::PageManager;
use crate::wal::log_manager::LogManager;
use crate::wal::log_record::{LogRecord, LogRecordBody, LogRecordType};
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
}

/// ARIES-style Crash Recovery Engine executing Analysis, Redo, and Undo passes.
pub struct RecoveryEngine;

impl RecoveryEngine {
    /// Executes full ARIES recovery on the given `PageManager` using the provided `LogManager`.
    pub fn recover(
        page_manager: &mut PageManager,
        log_manager: &mut LogManager,
    ) -> Result<RecoveryStats> {
        let mut stats = RecoveryStats::default();

        // Load all log records from the WAL
        let records = log_manager.read_all_records()?;
        stats.records_scanned = records.len();
        if records.is_empty() {
            return Ok(stats);
        }

        // Map LSN to LogRecord for random access during Undo pass
        let mut log_map: HashMap<Lsn, LogRecord> = HashMap::with_capacity(records.len());
        for rec in &records {
            log_map.insert(rec.lsn, rec.clone());
        }

        // -------------------------------------------------------------------
        // 1. ANALYSIS PASS
        // -------------------------------------------------------------------
        let mut txn_table: HashMap<TxnId, TxnTableEntry> = HashMap::new();

        for rec in &records {
            match rec.record_type {
                LogRecordType::Begin => {
                    txn_table.insert(
                        rec.txn_id,
                        TxnTableEntry {
                            status: TxnStatus::Active,
                            last_lsn: rec.lsn,
                            undo_next_lsn: rec.lsn,
                        },
                    );
                }
                LogRecordType::Commit => {
                    if let Some(entry) = txn_table.get_mut(&rec.txn_id) {
                        entry.status = TxnStatus::Committed;
                        entry.last_lsn = rec.lsn;
                    }
                }
                LogRecordType::Abort => {
                    if let Some(entry) = txn_table.get_mut(&rec.txn_id) {
                        entry.status = TxnStatus::Aborted;
                        entry.last_lsn = rec.lsn;
                    }
                }
                LogRecordType::PageDelta => {
                    let entry = txn_table.entry(rec.txn_id).or_insert(TxnTableEntry {
                        status: TxnStatus::Active,
                        last_lsn: rec.lsn,
                        undo_next_lsn: rec.lsn,
                    });
                    entry.last_lsn = rec.lsn;
                    entry.undo_next_lsn = rec.lsn;
                }
                LogRecordType::Clr => {
                    if let LogRecordBody::Clr { undo_next_lsn, .. } = &rec.body {
                        let entry = txn_table.entry(rec.txn_id).or_insert(TxnTableEntry {
                            status: TxnStatus::Active,
                            last_lsn: rec.lsn,
                            undo_next_lsn: *undo_next_lsn,
                        });
                        entry.last_lsn = rec.lsn;
                        entry.undo_next_lsn = *undo_next_lsn;
                    }
                }
                LogRecordType::Checkpoint => {
                    if let LogRecordBody::Checkpoint { active_txns } = &rec.body {
                        for &(txn_id, last_lsn) in active_txns {
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

        for entry in txn_table.values() {
            match entry.status {
                TxnStatus::Active => stats.loser_txns += 1,
                TxnStatus::Committed => stats.committed_txns += 1,
                TxnStatus::Aborted => {}
            }
        }
        stats.active_txns = txn_table.len();

        // -------------------------------------------------------------------
        // 2. REDO PASS
        // -------------------------------------------------------------------
        // Replay history forward for all PageDelta and Clr records
        for rec in &records {
            match &rec.body {
                LogRecordBody::PageDelta {
                    page_id,
                    offset,
                    after_image,
                    ..
                } if *page_id < page_manager.page_count() => {
                    let mut page = page_manager.read_page(*page_id)?;
                    if page.lsn() < rec.lsn {
                        let start = *offset as usize;
                        let end = start + after_image.len();
                        if end <= page.as_bytes().len() {
                            page.as_bytes_mut()[start..end].copy_from_slice(after_image);
                            page.set_lsn(rec.lsn);
                            page_manager.write_page(&page)?;
                            stats.redo_count += 1;
                        }
                    }
                }
                LogRecordBody::Clr {
                    page_id,
                    offset,
                    redo_image,
                    ..
                } if *page_id < page_manager.page_count() => {
                    let mut page = page_manager.read_page(*page_id)?;
                    if page.lsn() < rec.lsn {
                        let start = *offset as usize;
                        let end = start + redo_image.len();
                        if end <= page.as_bytes().len() {
                            page.as_bytes_mut()[start..end].copy_from_slice(redo_image);
                            page.set_lsn(rec.lsn);
                            page_manager.write_page(&page)?;
                            stats.redo_count += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        // -------------------------------------------------------------------
        // 3. UNDO PASS
        // -------------------------------------------------------------------
        // Roll back uncommitted (loser) transactions backward
        let mut active_undo_lsns: Vec<(Lsn, TxnId)> = txn_table
            .iter()
            .filter(|(_, entry)| {
                entry.status == TxnStatus::Active && entry.undo_next_lsn.is_valid()
            })
            .map(|(&txn_id, entry)| (entry.undo_next_lsn, txn_id))
            .collect();

        while !active_undo_lsns.is_empty() {
            // Find maximum LSN among active losers to undo in reverse chronological order
            active_undo_lsns.sort_by_key(|&(lsn, _)| lsn);
            let (curr_lsn, txn_id) = active_undo_lsns.pop().unwrap();

            if let Some(rec) = log_map.get(&curr_lsn) {
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
                        page.set_lsn(clr_lsn);
                        page_manager.write_page(&page)?;

                        stats.undo_count += 1;
                        stats.clrs_generated += 1;

                        if rec.prev_lsn.is_valid() {
                            active_undo_lsns.push((rec.prev_lsn, txn_id));
                        } else {
                            // Transaction undo complete, append Abort record
                            log_manager.abort_txn(txn_id)?;
                        }
                    }
                    LogRecordBody::Clr { undo_next_lsn, .. } => {
                        if undo_next_lsn.is_valid() {
                            active_undo_lsns.push((*undo_next_lsn, txn_id));
                        } else {
                            log_manager.abort_txn(txn_id)?;
                        }
                    }
                    LogRecordBody::Begin => {
                        // Reached beginning of transaction log chain
                        log_manager.abort_txn(txn_id)?;
                    }
                    _ => {}
                }
            }
        }

        // Flush WAL and page manager to ensure recovered state is persistent
        if log_manager.next_lsn().is_valid() {
            log_manager.flush(log_manager.next_lsn())?;
        }
        page_manager.sync()?;

        Ok(stats)
    }
}
