use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::page::PageId;
use crate::wal::log_record::{LogRecord, LogRecordBody, LogRecordType};
use crate::wal::lsn::{Lsn, TxnId, NULL_LSN};

/// Recognizable magic bytes for TreeDB WAL files.
pub const WAL_MAGIC_BYTES: &[u8; 8] = b"TREEWAL1";

/// Current WAL file format version.
pub const CURRENT_WAL_VERSION: u32 = 1;

/// Fixed header size of a TreeDB WAL file.
pub const WAL_HEADER_SIZE: usize = 32;

/// Persistent header stored at the start of a WAL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub flushed_lsn: Lsn,
}

impl WalHeader {
    /// Creates a newly initialized `WalHeader`.
    pub fn new() -> Self {
        Self {
            magic: *WAL_MAGIC_BYTES,
            version: CURRENT_WAL_VERSION,
            flushed_lsn: NULL_LSN,
        }
    }

    /// Serializes the WAL header into a 32-byte array.
    pub fn serialize(&self) -> [u8; WAL_HEADER_SIZE] {
        let mut bytes = [0u8; WAL_HEADER_SIZE];
        bytes[0..8].copy_from_slice(&self.magic);
        bytes[8..12].copy_from_slice(&self.version.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.flushed_lsn.to_bytes());
        bytes
    }

    /// Deserializes a `WalHeader` from a 32-byte slice.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < WAL_HEADER_SIZE {
            return Err(Error::SerializationFailure(
                "insufficient bytes for WAL header".into(),
            ));
        }

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&bytes[0..8]);
        if &magic != WAL_MAGIC_BYTES {
            return Err(Error::InvalidDatabase(format!(
                "invalid WAL magic bytes: expected {WAL_MAGIC_BYTES:?}, found {magic:?}"
            )));
        }

        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != CURRENT_WAL_VERSION {
            return Err(Error::UnsupportedFormatVersion {
                found: version,
                supported: CURRENT_WAL_VERSION,
            });
        }

        let flushed_lsn = Lsn::from_bytes(bytes[12..20].try_into().unwrap());
        Ok(Self {
            magic,
            version,
            flushed_lsn,
        })
    }
}

impl Default for WalHeader {
    fn default() -> Self {
        Self::new()
    }
}

/// WAL Log Manager responsible for appending log records, maintaining LSN ordering,
/// flushing to durable storage, and recovering log streams.
#[derive(Debug)]
pub struct LogManager {
    path: PathBuf,
    file: File,
    header: WalHeader,
    next_lsn: Lsn,
    flushed_lsn: Lsn,
    next_txn_id: TxnId,
    txn_first_lsn: HashMap<TxnId, Lsn>,
    txn_prev_lsn: HashMap<TxnId, Lsn>,
}

impl LogManager {
    /// Creates and initializes a new WAL file at `path`.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        let header = WalHeader::new();
        let header_bytes = header.serialize();
        file.write_all(&header_bytes)?;
        file.sync_all()?;

        Ok(Self {
            path,
            file,
            header,
            next_lsn: Lsn::new(1),
            flushed_lsn: NULL_LSN,
            next_txn_id: TxnId::new(1),
            txn_first_lsn: HashMap::new(),
            txn_prev_lsn: HashMap::new(),
        })
    }

    /// Opens an existing WAL file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // If crash happened during rename, restore from .bak if .wal is missing
        let bak_path = PathBuf::from(format!("{}.bak", path.display()));
        if !path.exists() && bak_path.exists() {
            std::fs::rename(&bak_path, &path)?;
        }
        let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
        if tmp_path.exists() {
            let _ = std::fs::remove_file(&tmp_path);
        }

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;

        let mut header_bytes = [0u8; WAL_HEADER_SIZE];
        file.read_exact(&mut header_bytes)?;
        let header = WalHeader::deserialize(&header_bytes)?;

        let mut manager = Self {
            path,
            file,
            header: header.clone(),
            next_lsn: Lsn::new(1),
            flushed_lsn: header.flushed_lsn,
            next_txn_id: TxnId::new(1),
            txn_first_lsn: HashMap::new(),
            txn_prev_lsn: HashMap::new(),
        };

        // Scan existing records to locate highest LSN, highest TxnId, and active transactions
        let records = manager.read_all_records()?;
        let mut active_first_lsn = HashMap::new();
        let mut active_prev_lsn = HashMap::new();

        for rec in &records {
            if rec.lsn >= manager.next_lsn {
                manager.next_lsn = rec.lsn.next();
            }
            if rec.txn_id >= manager.next_txn_id {
                manager.next_txn_id = rec.txn_id.next();
            }
            match rec.record_type {
                LogRecordType::Begin => {
                    active_first_lsn.insert(rec.txn_id, rec.lsn);
                    active_prev_lsn.insert(rec.txn_id, rec.lsn);
                }
                LogRecordType::Commit | LogRecordType::Abort => {
                    active_first_lsn.remove(&rec.txn_id);
                    active_prev_lsn.remove(&rec.txn_id);
                }
                LogRecordType::PageDelta | LogRecordType::Clr => {
                    active_first_lsn.entry(rec.txn_id).or_insert(rec.lsn);
                    active_prev_lsn.insert(rec.txn_id, rec.lsn);
                }
                LogRecordType::Checkpoint => {
                    if let LogRecordBody::Checkpoint { active_txns } = &rec.body {
                        for &(tid, last_lsn) in active_txns {
                            active_first_lsn.entry(tid).or_insert(last_lsn);
                            active_prev_lsn.insert(tid, last_lsn);
                        }
                    }
                }
            }
        }
        manager.txn_first_lsn = active_first_lsn;
        manager.txn_prev_lsn = active_prev_lsn;

        // Position file pointer at end of log
        manager.file.seek(SeekFrom::End(0))?;

        Ok(manager)
    }

    /// Allocates a new transaction ID and appends a `Begin` log record.
    pub fn begin_txn(&mut self) -> Result<TxnId> {
        let txn_id = self.next_txn_id;
        self.next_txn_id = self.next_txn_id.next();

        let lsn = self.allocate_lsn();
        let record = LogRecord::new_begin(lsn, txn_id);
        self.append_record(&record)?;

        self.txn_first_lsn.insert(txn_id, lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        Ok(txn_id)
    }

    /// Appends a `Commit` log record for `txn_id` and flushes the WAL through the commit LSN.
    pub fn commit_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        let prev_lsn = self.txn_prev_lsn.get(&txn_id).copied().unwrap_or(NULL_LSN);
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_commit(lsn, prev_lsn, txn_id);
        self.append_record(&record)?;

        self.txn_first_lsn.remove(&txn_id);
        self.txn_prev_lsn.remove(&txn_id);
        self.flush(lsn)?;
        Ok(lsn)
    }

    /// Appends an `Abort` log record for `txn_id` and flushes the WAL through the abort LSN.
    pub fn abort_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        let prev_lsn = self.txn_prev_lsn.get(&txn_id).copied().unwrap_or(NULL_LSN);
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_abort(lsn, prev_lsn, txn_id);
        self.append_record(&record)?;

        self.txn_first_lsn.remove(&txn_id);
        self.txn_prev_lsn.remove(&txn_id);
        self.flush(lsn)?;
        Ok(lsn)
    }

    /// Appends a `PageDelta` log record representing physical changes to a database page.
    pub fn append_page_delta(
        &mut self,
        txn_id: TxnId,
        page_id: PageId,
        offset: u16,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    ) -> Result<Lsn> {
        let prev_lsn = self.txn_prev_lsn.get(&txn_id).copied().unwrap_or(NULL_LSN);
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_page_delta(
            lsn,
            prev_lsn,
            txn_id,
            page_id,
            offset,
            before_image,
            after_image,
        );
        self.append_record(&record)?;
        self.txn_first_lsn.entry(txn_id).or_insert(lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        Ok(lsn)
    }

    /// Appends a Compensation Log Record (CLR) during the undo recovery pass.
    pub fn append_clr(
        &mut self,
        txn_id: TxnId,
        page_id: PageId,
        offset: u16,
        redo_image: Vec<u8>,
        undo_next_lsn: Lsn,
    ) -> Result<Lsn> {
        let prev_lsn = self.txn_prev_lsn.get(&txn_id).copied().unwrap_or(NULL_LSN);
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_clr(
            lsn,
            prev_lsn,
            txn_id,
            page_id,
            offset,
            redo_image,
            undo_next_lsn,
        );
        self.append_record(&record)?;
        self.txn_first_lsn.entry(txn_id).or_insert(lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        Ok(lsn)
    }

    /// Appends a `Checkpoint` log record capturing currently active transactions.
    pub fn append_checkpoint(&mut self, active_txns: Vec<(TxnId, Lsn)>) -> Result<Lsn> {
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_checkpoint(lsn, active_txns);
        self.append_record(&record)?;
        self.flush(lsn)?;
        Ok(lsn)
    }

    /// Flushes all WAL records up to `target_lsn` to durable physical storage.
    pub fn flush(&mut self, target_lsn: Lsn) -> Result<()> {
        if target_lsn <= self.flushed_lsn && self.flushed_lsn.is_valid() {
            return Ok(());
        }

        self.flushed_lsn = target_lsn;
        self.header.flushed_lsn = target_lsn;

        // Seek to header and update flushed_lsn
        let current_pos = self.file.stream_position()?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.serialize())?;
        self.file.sync_all()?;
        self.file.seek(SeekFrom::Start(current_pos))?;

        Ok(())
    }

    /// Sequentially scans and reads all valid log records in the WAL file.
    ///
    /// Distinguishes between un-flushed crash-truncated tails (which are gracefully ignored)
    /// and mid-log corruption within the durable prefix (which returns `Error::WalCorruption`).
    pub fn read_all_records(&mut self) -> Result<Vec<LogRecord>> {
        self.file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        let mut buffer = Vec::new();
        self.file.read_to_end(&mut buffer)?;

        let mut records = Vec::new();
        let mut offset = 0;
        let mut last_lsn = NULL_LSN;

        while offset < buffer.len() {
            let slice = &buffer[offset..];
            match LogRecord::deserialize(slice) {
                Ok((record, consumed)) => {
                    if record.lsn <= last_lsn {
                        return Err(Error::WalCorruption(format!(
                            "LSN monotonicity violation: record LSN {} <= previous LSN {}",
                            record.lsn, last_lsn
                        )));
                    }
                    last_lsn = record.lsn;
                    records.push(record);
                    offset += consumed;
                }
                Err(err) => {
                    // Check if corruption occurred within the durable prefix
                    if self.header.flushed_lsn.is_valid() && last_lsn < self.header.flushed_lsn {
                        return Err(Error::WalCorruption(format!(
                            "Durable WAL corrupted before flushed_lsn {}: {err}",
                            self.header.flushed_lsn
                        )));
                    }
                    // Otherwise, stop gracefully on un-flushed trailing fragment
                    break;
                }
            }
        }

        Ok(records)
    }

    /// Returns a list of all currently active transactions and their last logged LSNs.
    pub fn active_txns(&self) -> Vec<(TxnId, Lsn)> {
        let mut list: Vec<(TxnId, Lsn)> = self
            .txn_prev_lsn
            .iter()
            .map(|(&tid, &lsn)| (tid, lsn))
            .collect();
        list.sort_by_key(|(tid, _)| *tid);
        list
    }

    /// Returns the minimum logged LSN across all active transactions (or `None` if no active transactions).
    pub fn min_active_txn_lsn(&self) -> Option<Lsn> {
        self.txn_first_lsn.values().copied().min()
    }

    /// Calculates the minimum LSN that must be retained in the WAL.
    pub fn calculate_retention_lsn(&self, last_checkpoint_lsn: Lsn) -> Lsn {
        if !last_checkpoint_lsn.is_valid() {
            return Lsn(1);
        }
        match self.min_active_txn_lsn() {
            Some(min_active) => std::cmp::min(min_active, last_checkpoint_lsn),
            None => last_checkpoint_lsn,
        }
    }

    /// Truncates the WAL prefix by safely removing records with `LSN < min_lsn`.
    ///
    /// Retained records (`LSN >= min_lsn`) are written to a temporary file, synced,
    /// and replaced crash-safely.
    /// Returns the number of obsolete log records removed.
    pub fn truncate_prefix(&mut self, min_lsn: Lsn) -> Result<usize> {
        if !min_lsn.is_valid() || min_lsn <= Lsn(1) {
            return Ok(0);
        }

        let all_records = self.read_all_records()?;
        let total_count = all_records.len();
        let retained: Vec<LogRecord> = all_records
            .into_iter()
            .filter(|r| r.lsn >= min_lsn)
            .collect();
        let removed_count = total_count - retained.len();

        if removed_count == 0 {
            return Ok(0);
        }

        let tmp_path = PathBuf::from(format!("{}.tmp", self.path.display()));
        let bak_path = PathBuf::from(format!("{}.bak", self.path.display()));

        // 1. Write retained records to temp file
        {
            let mut tmp_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;

            let header_bytes = self.header.serialize();
            tmp_file.write_all(&header_bytes)?;
            for rec in &retained {
                let rec_bytes = rec.serialize();
                tmp_file.write_all(&rec_bytes)?;
            }
            tmp_file.sync_all()?;
        }

        // 2. Safely replace WAL file releasing file handles to avoid OS sharing violations
        let scratch_path = PathBuf::from(format!("{}.scratch", self.path.display()));
        let scratch_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch_path)?;
        let _old_file = std::mem::replace(&mut self.file, scratch_file);
        drop(_old_file);

        // Backup existing WAL
        if self.path.exists() {
            let _ = std::fs::remove_file(&bak_path);
            std::fs::rename(&self.path, &bak_path)?;
        }

        // Rename tmp to path
        std::fs::rename(&tmp_path, &self.path)?;
        let _ = std::fs::remove_file(&bak_path);

        // 3. Reopen self.file on the updated self.path
        self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        self.file.seek(SeekFrom::End(0))?;
        let _ = std::fs::remove_file(&scratch_path);

        Ok(removed_count)
    }

    /// Returns the currently flushed LSN.
    pub const fn flushed_lsn(&self) -> Lsn {
        self.flushed_lsn
    }

    /// Returns the next LSN to be allocated.
    pub const fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// Returns the path to the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // --- Internal Helpers ---

    fn allocate_lsn(&mut self) -> Lsn {
        let lsn = self.next_lsn;
        self.next_lsn = self.next_lsn.next();
        lsn
    }

    fn append_record(&mut self, record: &LogRecord) -> Result<()> {
        let bytes = record.serialize();
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::log_record::LogRecordType;
    use tempfile::NamedTempFile;

    #[test]
    fn test_wal_lifecycle_and_record_persistence() {
        let temp_file = NamedTempFile::new().unwrap();
        let wal_path = temp_file.path().to_path_buf();

        let mut manager = LogManager::create(&wal_path).unwrap();
        assert_eq!(manager.flushed_lsn(), NULL_LSN);
        assert_eq!(manager.next_lsn(), Lsn(1));

        let t1 = manager.begin_txn().unwrap();
        let lsn_delta = manager
            .append_page_delta(t1, 4, 10, vec![1, 2], vec![3, 4])
            .unwrap();
        let lsn_commit = manager.commit_txn(t1).unwrap();

        assert_eq!(manager.flushed_lsn(), lsn_commit);
        drop(manager);

        // Reopen WAL
        let mut reopened = LogManager::open(&wal_path).unwrap();
        assert_eq!(reopened.flushed_lsn(), lsn_commit);
        let records = reopened.read_all_records().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].record_type, LogRecordType::Begin);
        assert_eq!(records[1].record_type, LogRecordType::PageDelta);
        assert_eq!(records[2].record_type, LogRecordType::Commit);
        assert_eq!(records[1].lsn, lsn_delta);
        assert_eq!(records[2].lsn, lsn_commit);
    }
}
