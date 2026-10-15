use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::wal::log_record::LogRecord;
use crate::wal::lsn::{Lsn, NULL_LSN};

/// Default buffer capacity (64 KB) for streaming WAL reader.
pub const DEFAULT_WAL_BUFFER_SIZE: usize = 64 * 1024;

/// Minimum valid serialized record payload length in bytes:
/// `lsn(8) + prev_lsn(8) + txn_id(8) + record_type(1) + crc32(4) = 29`.
pub const MIN_WAL_RECORD_PAYLOAD_LEN: usize = 29;

/// Maximum allowed payload size for a single WAL record (16 MB).
/// Protects against excessive allocation from corrupted length prefixes.
pub const MAX_WAL_RECORD_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

/// Incremental, bounded-memory streaming reader for TreeDB write-ahead log files.
///
/// Reads and deserializes `LogRecord`s sequentially using a sliding memory buffer.
/// Correctly handles records spanning buffer boundaries, verifies CRC32 integrity,
/// enforces monotonic LSN ordering, and distinguishes between durable mid-log corruption
/// and un-flushed crash-truncated trailing fragments.
pub struct WalReader<R> {
    inner: R,
    buffer: Vec<u8>,
    read_pos: usize,
    valid_len: usize,
    stream_offset: u64,
    flushed_lsn: Lsn,
    last_lsn: Lsn,
    enforce_monotonicity: bool,
    initial_buffer_size: usize,
}

impl<R: Read + Seek> WalReader<R> {
    /// Creates a new `WalReader` wrapping `inner` with the default buffer capacity (64 KB).
    pub fn new(inner: R, flushed_lsn: Lsn) -> Result<Self> {
        Self::with_buffer_size(inner, flushed_lsn, DEFAULT_WAL_BUFFER_SIZE)
    }

    /// Creates a new `WalReader` wrapping `inner` with an explicit buffer capacity.
    pub fn with_buffer_size(mut inner: R, flushed_lsn: Lsn, buffer_size: usize) -> Result<Self> {
        let buf_size = buffer_size.max(MIN_WAL_RECORD_PAYLOAD_LEN + 4);
        let stream_offset = inner.stream_position()?;
        Ok(Self {
            inner,
            buffer: vec![0u8; buf_size],
            read_pos: 0,
            valid_len: 0,
            stream_offset,
            flushed_lsn,
            last_lsn: NULL_LSN,
            enforce_monotonicity: true,
            initial_buffer_size: buf_size,
        })
    }

    /// Returns the currently tracked flushed LSN boundary.
    pub const fn flushed_lsn(&self) -> Lsn {
        self.flushed_lsn
    }

    /// Returns the LSN of the last valid record read.
    pub const fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    /// Returns the exact current file offset of the next unparsed byte.
    pub const fn current_stream_offset(&self) -> u64 {
        self.stream_offset
    }

    /// Configures whether monotonic LSN ordering is strictly enforced between records.
    pub fn set_enforce_monotonicity(&mut self, enforce: bool) {
        self.enforce_monotonicity = enforce;
    }

    /// Resets the stream position and internal buffer state to seek to `target_offset`.
    pub fn seek_to_offset(&mut self, target_offset: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Start(target_offset))?;
        self.read_pos = 0;
        self.valid_len = 0;
        self.stream_offset = target_offset;
        self.enforce_monotonicity = false;
        Ok(())
    }

    /// Reads the next valid log record, returning `Ok(None)` on clean EOF or safe un-flushed tail.
    pub fn next_record(&mut self) -> Result<Option<LogRecord>> {
        self.next_record_with_offset()
            .map(|opt| opt.map(|(rec, _, _)| rec))
    }

    /// Reads the next valid log record along with its starting file offset and consumed byte length.
    pub fn next_record_with_offset(&mut self) -> Result<Option<(LogRecord, u64, usize)>> {
        // Step 1: Ensure at least 4 bytes are buffered for the payload length prefix
        if !self.ensure_buffered(4)? {
            let unparsed = self.valid_len - self.read_pos;
            if unparsed == 0 {
                self.shrink_if_needed();
                return Ok(None);
            }
            if self.is_durable_corruption() {
                return Err(Error::WalCorruption(format!(
                    "Durable WAL corrupted before flushed_lsn {}: truncated length prefix at offset {}",
                    self.flushed_lsn, self.stream_offset
                )));
            }
            return Ok(None);
        }

        // Step 2: Decode payload length prefix
        let payload_len = u32::from_le_bytes([
            self.buffer[self.read_pos],
            self.buffer[self.read_pos + 1],
            self.buffer[self.read_pos + 2],
            self.buffer[self.read_pos + 3],
        ]) as usize;

        if payload_len < MIN_WAL_RECORD_PAYLOAD_LEN {
            if self.is_durable_corruption()
                || self.has_bytes_after(4 + MIN_WAL_RECORD_PAYLOAD_LEN)?
            {
                return Err(Error::WalCorruption(format!(
                    "WAL corrupted at offset {}: invalid payload length {payload_len}",
                    self.stream_offset
                )));
            }
            return Ok(None);
        }

        if payload_len > MAX_WAL_RECORD_PAYLOAD_LEN {
            if self.is_durable_corruption()
                || self.has_bytes_after(4 + MIN_WAL_RECORD_PAYLOAD_LEN)?
            {
                return Err(Error::WalCorruption(format!(
                    "WAL corrupted at offset {}: payload length {payload_len} exceeds maximum allowed {MAX_WAL_RECORD_PAYLOAD_LEN}",
                    self.stream_offset
                )));
            }
            return Ok(None);
        }

        let total_record_len = 4 + payload_len;

        // Step 3: Ensure the entire serialized record is available in buffer
        if !self.ensure_buffered(total_record_len)? {
            let available = self.valid_len - self.read_pos;
            if self.is_durable_corruption() {
                return Err(Error::WalCorruption(format!(
                    "Durable WAL corrupted before flushed_lsn {}: truncated log record (expected {total_record_len} bytes, available {available}) at offset {}",
                    self.flushed_lsn, self.stream_offset
                )));
            }
            return Ok(None);
        }

        // Step 4: Deserialize and validate record integrity
        let record_slice = &self.buffer[self.read_pos..self.read_pos + total_record_len];
        let (record, consumed) = match LogRecord::deserialize(record_slice) {
            Ok(res) => res,
            Err(err) => {
                if self.is_durable_corruption() || self.has_bytes_after(total_record_len)? {
                    return Err(Error::WalCorruption(format!(
                        "WAL corrupted at offset {}: {err}",
                        self.stream_offset
                    )));
                }
                return Ok(None);
            }
        };

        if self.enforce_monotonicity && self.last_lsn.is_valid() && record.lsn <= self.last_lsn {
            return Err(Error::WalCorruption(format!(
                "LSN monotonicity violation: record LSN {} <= previous LSN {}",
                record.lsn, self.last_lsn
            )));
        }

        let record_offset = self.stream_offset;
        self.last_lsn = record.lsn;
        self.read_pos += consumed;
        self.stream_offset += consumed as u64;

        self.shrink_if_needed();

        Ok(Some((record, record_offset, consumed)))
    }

    /// Reads a single log record starting precisely at `offset`.
    pub fn read_record_at(&mut self, offset: u64) -> Result<LogRecord> {
        self.seek_to_offset(offset)?;
        self.next_record()?.ok_or_else(|| {
            Error::WalCorruption(format!("no valid log record found at WAL offset {offset}"))
        })
    }

    // --- Internal Helpers ---

    /// Ensures that at least `needed` contiguous bytes are present starting at `self.read_pos`.
    /// Returns `Ok(true)` if available, or `Ok(false)` if EOF was encountered before reading `needed` bytes.
    fn ensure_buffered(&mut self, needed: usize) -> Result<bool> {
        let unparsed = self.valid_len.saturating_sub(self.read_pos);
        if unparsed >= needed {
            return Ok(true);
        }

        // Compact remaining unparsed bytes to buffer start
        if self.read_pos > 0 {
            if unparsed > 0 {
                self.buffer.copy_within(self.read_pos..self.valid_len, 0);
            }
            self.valid_len = unparsed;
            self.read_pos = 0;
        }

        // Resize buffer if the record exceeds current capacity
        if self.buffer.len() < needed {
            self.buffer.resize(needed, 0);
        }

        // Read until we have at least `needed` bytes or EOF
        while self.valid_len < needed {
            let n = self.inner.read(&mut self.buffer[self.valid_len..])?;
            if n == 0 {
                return Ok(false);
            }
            self.valid_len += n;
        }

        Ok(true)
    }

    /// Returns `true` if current scan position has not yet reached the durable `flushed_lsn`.
    fn is_durable_corruption(&self) -> bool {
        self.flushed_lsn.is_valid() && self.last_lsn < self.flushed_lsn
    }

    /// Returns `true` if there are subsequent bytes available in the buffer or underlying file
    /// beyond `consumed_bytes` from `self.read_pos`.
    fn has_bytes_after(&mut self, consumed_bytes: usize) -> Result<bool> {
        let unparsed = self.valid_len.saturating_sub(self.read_pos);
        if unparsed > consumed_bytes {
            return Ok(true);
        }
        let curr_pos = self.inner.stream_position()?;
        let end_pos = self.inner.seek(SeekFrom::End(0))?;
        self.inner.seek(SeekFrom::Start(curr_pos))?;
        let bytes_in_file = end_pos.saturating_sub(curr_pos);
        Ok(bytes_in_file + unparsed as u64 > consumed_bytes as u64)
    }

    /// Shrinks the buffer back to `initial_buffer_size` if it was temporarily grown for a large record.
    fn shrink_if_needed(&mut self) {
        if self.read_pos == self.valid_len {
            self.read_pos = 0;
            self.valid_len = 0;
            if self.buffer.len() > self.initial_buffer_size {
                self.buffer.resize(self.initial_buffer_size, 0);
                self.buffer.shrink_to(self.initial_buffer_size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::lsn::TxnId;
    use std::io::Cursor;

    fn make_test_record(lsn: Lsn, txn_id: TxnId, payload_size: usize) -> LogRecord {
        let before = vec![0x11; payload_size];
        let after = vec![0x22; payload_size];
        LogRecord::new_page_delta(lsn, NULL_LSN, txn_id, 2, 64, before, after)
    }

    #[test]
    fn test_streaming_reader_small_buffer_spanning_boundaries() {
        // Test records that span across buffer boundaries with a small 64-byte buffer
        let mut wal_data = Vec::new();
        let r1 = make_test_record(Lsn(1), TxnId(1), 10);
        let r2 = make_test_record(Lsn(2), TxnId(1), 50); // Large record that spans across 64B
        let r3 = make_test_record(Lsn(3), TxnId(1), 20);

        wal_data.extend_from_slice(&r1.serialize());
        wal_data.extend_from_slice(&r2.serialize());
        wal_data.extend_from_slice(&r3.serialize());

        let cursor = Cursor::new(wal_data);
        let mut reader = WalReader::with_buffer_size(cursor, Lsn(3), 64).unwrap();

        let rec1 = reader.next_record().unwrap().expect("record 1");
        assert_eq!(rec1.lsn, Lsn(1));
        let rec2 = reader.next_record().unwrap().expect("record 2");
        assert_eq!(rec2.lsn, Lsn(2));
        let rec3 = reader.next_record().unwrap().expect("record 3");
        assert_eq!(rec3.lsn, Lsn(3));
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn test_streaming_reader_unflushed_tail_gracefully_ignored() {
        let mut wal_data = Vec::new();
        let r1 = make_test_record(Lsn(1), TxnId(1), 16);
        let r2 = make_test_record(Lsn(2), TxnId(1), 16);
        wal_data.extend_from_slice(&r1.serialize());
        wal_data.extend_from_slice(&r2.serialize());

        // Append 10 bytes of truncated trailing write
        wal_data.extend_from_slice(&[0x05, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC]);

        // flushed_lsn is Lsn(2): the trailing write was never flushed before crash
        let cursor = Cursor::new(wal_data);
        let mut reader = WalReader::with_buffer_size(cursor, Lsn(2), 64).unwrap();

        assert_eq!(reader.next_record().unwrap().unwrap().lsn, Lsn(1));
        assert_eq!(reader.next_record().unwrap().unwrap().lsn, Lsn(2));
        // Must return Ok(None) gracefully for unflushed trailing bytes
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn test_streaming_reader_durable_corruption_rejected() {
        let mut wal_data = Vec::new();
        let r1 = make_test_record(Lsn(1), TxnId(1), 16);
        let r2 = make_test_record(Lsn(2), TxnId(1), 16);
        let mut r1_bytes = r1.serialize();
        // Corrupt CRC32 in record 1
        let last_idx = r1_bytes.len() - 1;
        r1_bytes[last_idx] ^= 0xFF;

        wal_data.extend_from_slice(&r1_bytes);
        wal_data.extend_from_slice(&r2.serialize());

        // flushed_lsn is Lsn(2): corruption is within the durable prefix
        let cursor = Cursor::new(wal_data);
        let mut reader = WalReader::with_buffer_size(cursor, Lsn(2), 64).unwrap();

        let err = reader.next_record().unwrap_err();
        assert!(matches!(err, Error::WalCorruption(_)));
    }

    #[test]
    fn test_streaming_reader_monotonicity_enforced() {
        let mut wal_data = Vec::new();
        let r1 = make_test_record(Lsn(5), TxnId(1), 16);
        let r2 = make_test_record(Lsn(3), TxnId(1), 16); // Non-monotonic LSN 3 <= 5
        wal_data.extend_from_slice(&r1.serialize());
        wal_data.extend_from_slice(&r2.serialize());

        let cursor = Cursor::new(wal_data);
        let mut reader = WalReader::new(cursor, Lsn(5)).unwrap();

        assert_eq!(reader.next_record().unwrap().unwrap().lsn, Lsn(5));
        let err = reader.next_record().unwrap_err();
        assert!(matches!(err, Error::WalCorruption(_)));
    }
}
