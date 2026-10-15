use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::page::PageId;
use crate::wal::log_record::{LogRecord, LogRecordBody, LogRecordType};
use crate::wal::lsn::{Lsn, TxnId, NULL_LSN};
use crate::wal::reader::WalReader;

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
    txn_record_offsets: HashMap<TxnId, HashMap<Lsn, u64>>,
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
            txn_record_offsets: HashMap::new(),
        })
    }

    /// Opens an existing WAL file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // If crash happened during rename, restore from .bak if .wal is missing or truncated
        let bak_path = PathBuf::from(format!("{}.bak", path.display()));
        let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
        if tmp_path.exists() {
            let _ = std::fs::remove_file(&tmp_path);
        }

        if !path.exists() && bak_path.exists() {
            std::fs::rename(&bak_path, &path)?;
        }

        let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                if len < WAL_HEADER_SIZE as u64 && bak_path.exists() {
                    drop(f);
                    let _ = std::fs::remove_file(&path);
                    std::fs::rename(&bak_path, &path)?;
                    OpenOptions::new().read(true).write(true).open(&path)?
                } else {
                    f
                }
            }
            Err(_) if bak_path.exists() => {
                std::fs::rename(&bak_path, &path)?;
                OpenOptions::new().read(true).write(true).open(&path)?
            }
            Err(e) => return Err(e.into()),
        };

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
            txn_record_offsets: HashMap::new(),
        };

        // Incrementally stream existing records to locate highest LSN, highest TxnId, and active transactions
        let mut reader = WalReader::new(&mut manager.file, header.flushed_lsn)?;
        let mut active_first_lsn = HashMap::new();
        let mut active_prev_lsn = HashMap::new();
        let mut txn_record_offsets: HashMap<TxnId, HashMap<Lsn, u64>> = HashMap::new();

        while let Some((rec, rec_offset, _)) = reader.next_record_with_offset()? {
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
                    txn_record_offsets
                        .entry(rec.txn_id)
                        .or_default()
                        .insert(rec.lsn, rec_offset);
                }
                LogRecordType::Commit | LogRecordType::Abort => {
                    active_first_lsn.remove(&rec.txn_id);
                    active_prev_lsn.remove(&rec.txn_id);
                    txn_record_offsets.remove(&rec.txn_id);
                }
                LogRecordType::PageDelta | LogRecordType::Clr => {
                    active_first_lsn.entry(rec.txn_id).or_insert(rec.lsn);
                    active_prev_lsn.insert(rec.txn_id, rec.lsn);
                    txn_record_offsets
                        .entry(rec.txn_id)
                        .or_default()
                        .insert(rec.lsn, rec_offset);
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
        manager.txn_record_offsets = txn_record_offsets;

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
        let offset = self.append_record(&record)?;

        self.txn_first_lsn.insert(txn_id, lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        self.txn_record_offsets
            .entry(txn_id)
            .or_default()
            .insert(lsn, offset);
        Ok(txn_id)
    }

    /// Appends a `Commit` log record for `txn_id` and flushes the WAL through the commit LSN.
    pub fn commit_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        self.commit_txn_with_seq(txn_id, 0)
    }

    /// Appends a `Commit` log record for `txn_id` with an explicit commit sequence and flushes the WAL.
    pub fn commit_txn_with_seq(&mut self, txn_id: TxnId, commit_seq: u64) -> Result<Lsn> {
        self.commit_txn_with_seq_and_fault(txn_id, commit_seq, None)
    }

    /// Appends a `Commit` log record for `txn_id` with an explicit commit sequence and optional fault injection.
    pub fn commit_txn_with_seq_and_fault(
        &mut self,
        txn_id: TxnId,
        commit_seq: u64,
        fault: Option<crate::engine::failure::StorageFailurePoint>,
    ) -> Result<Lsn> {
        if fault == Some(crate::engine::failure::StorageFailurePoint::BeforeCommitRecordLogged) {
            return Err(Error::InjectedFault(
                crate::engine::failure::StorageFailurePoint::BeforeCommitRecordLogged,
            ));
        }

        let prev_lsn = self.txn_prev_lsn.get(&txn_id).copied().unwrap_or(NULL_LSN);
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_commit_with_seq(lsn, prev_lsn, txn_id, commit_seq);
        let current_pos = self.file.stream_position()?;
        self.append_record(&record)?;

        if fault
            == Some(crate::engine::failure::StorageFailurePoint::AfterCommitRecordLoggedBeforeFlush)
        {
            // An un-flushed commit record before a crash does not reach durable physical media.
            // Truncate the file back to simulate the unflushed tail being lost on crash.
            self.file.set_len(current_pos)?;
            self.file.seek(SeekFrom::Start(current_pos))?;
            return Err(Error::InjectedFault(
                crate::engine::failure::StorageFailurePoint::AfterCommitRecordLoggedBeforeFlush,
            ));
        }

        self.txn_first_lsn.remove(&txn_id);
        self.txn_prev_lsn.remove(&txn_id);
        self.txn_record_offsets.remove(&txn_id);

        self.flush(lsn)?;

        if fault == Some(crate::engine::failure::StorageFailurePoint::AfterDurableCommitRecordFlush)
        {
            return Err(Error::InjectedFault(
                crate::engine::failure::StorageFailurePoint::AfterDurableCommitRecordFlush,
            ));
        }

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
        self.txn_record_offsets.remove(&txn_id);
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
        let pos = self.append_record(&record)?;
        self.txn_first_lsn.entry(txn_id).or_insert(lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        self.txn_record_offsets
            .entry(txn_id)
            .or_default()
            .insert(lsn, pos);
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
        let pos = self.append_record(&record)?;
        self.txn_first_lsn.entry(txn_id).or_insert(lsn);
        self.txn_prev_lsn.insert(txn_id, lsn);
        self.txn_record_offsets
            .entry(txn_id)
            .or_default()
            .insert(lsn, pos);
        Ok(lsn)
    }

    /// Appends a `Checkpoint` log record capturing currently active transactions.
    pub fn append_checkpoint(&mut self, active_txns: Vec<(TxnId, Lsn)>) -> Result<Lsn> {
        self.append_checkpoint_with_fault(active_txns, None)
    }

    /// Appends a `Checkpoint` log record capturing currently active transactions with optional failure injection.
    pub fn append_checkpoint_with_fault(
        &mut self,
        active_txns: Vec<(TxnId, Lsn)>,
        fault: Option<crate::engine::failure::StorageFailurePoint>,
    ) -> Result<Lsn> {
        let lsn = self.allocate_lsn();
        let record = LogRecord::new_checkpoint(lsn, active_txns);
        let current_pos = self.file.stream_position()?;
        self.append_record(&record)?;

        if fault == Some(crate::engine::failure::StorageFailurePoint::DuringCheckpointWalFlush) {
            // An un-flushed checkpoint record before a crash does not reach durable physical media.
            // Truncate the file back to simulate the unflushed tail being lost on crash.
            self.file.set_len(current_pos)?;
            self.file.seek(SeekFrom::Start(current_pos))?;
            return Err(Error::InjectedFault(
                crate::engine::failure::StorageFailurePoint::DuringCheckpointWalFlush,
            ));
        }

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
        let mut reader = self.reader()?;
        let mut records = Vec::new();
        while let Some(rec) = reader.next_record()? {
            records.push(rec);
        }
        self.file.seek(SeekFrom::End(0))?;
        Ok(records)
    }

    /// Returns a streaming WAL reader over this log manager's underlying file with default buffer capacity.
    pub fn reader(&mut self) -> Result<WalReader<&mut File>> {
        self.file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        WalReader::new(&mut self.file, self.header.flushed_lsn)
    }

    /// Returns a streaming WAL reader with an explicit buffer capacity.
    pub fn reader_with_buffer_size(&mut self, buffer_size: usize) -> Result<WalReader<&mut File>> {
        self.file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        WalReader::with_buffer_size(&mut self.file, self.header.flushed_lsn, buffer_size)
    }

    /// Looks up the recorded file offset for `(txn_id, lsn)` among active transactions.
    pub fn get_record_offset(&self, txn_id: TxnId, lsn: Lsn) -> Option<u64> {
        self.txn_record_offsets
            .get(&txn_id)
            .and_then(|m| m.get(&lsn).copied())
    }

    /// Reads a single log record starting precisely at `offset`.
    pub fn read_record_at_offset(&mut self, offset: u64) -> Result<LogRecord> {
        let mut reader = self.reader()?;
        let rec = reader.read_record_at(offset)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(rec)
    }

    /// Reads a log record for an active transaction by looking up its recorded offset.
    pub fn read_record_for_txn(&mut self, txn_id: TxnId, lsn: Lsn) -> Result<Option<LogRecord>> {
        if let Some(offset) = self.get_record_offset(txn_id, lsn) {
            return Ok(Some(self.read_record_at_offset(offset)?));
        }
        Ok(None)
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

    /// Diagnostic helper returning the current WAL retention LSN for a given checkpoint LSN.
    pub fn retention_lsn(&self, last_checkpoint_lsn: Lsn) -> Lsn {
        self.calculate_retention_lsn(last_checkpoint_lsn)
    }

    /// Truncates the WAL prefix by safely removing records with `LSN < min_lsn`.
    ///
    /// Retained records (`LSN >= min_lsn`) are written to a temporary file incrementally, synced,
    /// and replaced crash-safely without loading all records into memory.
    /// Returns the number of obsolete log records removed.
    pub fn truncate_prefix(&mut self, min_lsn: Lsn) -> Result<usize> {
        if !min_lsn.is_valid() || min_lsn <= Lsn(1) {
            return Ok(0);
        }

        let tmp_path = PathBuf::from(format!("{}.tmp", self.path.display()));
        let bak_path = PathBuf::from(format!("{}.bak", self.path.display()));

        let mut removed_count = 0;

        // 1. Write retained records to temp file streaming incrementally
        {
            let mut tmp_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;

            let header_bytes = self.header.serialize();
            tmp_file.write_all(&header_bytes)?;

            let mut reader = self.reader()?;
            while let Some(rec) = reader.next_record()? {
                if rec.lsn < min_lsn {
                    removed_count += 1;
                } else {
                    tmp_file.write_all(&rec.serialize())?;
                }
            }
            tmp_file.sync_all()?;
        }

        if removed_count == 0 {
            let _ = std::fs::remove_file(&tmp_path);
            return Ok(0);
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

        // 4. Rebuild active transaction record offsets since file positions shifted
        self.rebuild_active_txn_offsets()?;

        Ok(removed_count)
    }

    fn rebuild_active_txn_offsets(&mut self) -> Result<()> {
        self.txn_record_offsets.clear();
        self.file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        let mut reader = WalReader::new(&mut self.file, self.header.flushed_lsn)?;
        while let Some((rec, offset, _)) = reader.next_record_with_offset()? {
            match rec.record_type {
                LogRecordType::Begin | LogRecordType::PageDelta | LogRecordType::Clr => {
                    if self.txn_prev_lsn.contains_key(&rec.txn_id) {
                        self.txn_record_offsets
                            .entry(rec.txn_id)
                            .or_default()
                            .insert(rec.lsn, offset);
                    }
                }
                LogRecordType::Commit | LogRecordType::Abort => {
                    self.txn_record_offsets.remove(&rec.txn_id);
                }
                _ => {}
            }
        }
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
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

    /// Returns a reference to the active transactions' previous LSN mapping.
    pub fn txn_prev_lsn(&self) -> &HashMap<TxnId, Lsn> {
        &self.txn_prev_lsn
    }

    /// Returns a reference to the active transactions' first LSN mapping.
    pub fn txn_first_lsn(&self) -> &HashMap<TxnId, Lsn> {
        &self.txn_first_lsn
    }

    // --- Internal Helpers ---

    fn allocate_lsn(&mut self) -> Lsn {
        let lsn = self.next_lsn;
        self.next_lsn = self.next_lsn.next();
        lsn
    }

    fn append_record(&mut self, record: &LogRecord) -> Result<u64> {
        let bytes = record.serialize();
        self.file.seek(SeekFrom::End(0))?;
        let offset = self.file.stream_position()?;
        self.file.write_all(&bytes)?;
        Ok(offset)
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
