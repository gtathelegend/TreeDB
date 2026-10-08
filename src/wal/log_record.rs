use crate::error::{Error, Result};
use crate::storage::page::PageId;
use crate::wal::lsn::{Lsn, TxnId, NULL_LSN, NULL_TXN};

/// Discriminant tag identifying log record semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LogRecordType {
    Begin = 1,
    Commit = 2,
    Abort = 3,
    PageDelta = 4,
    Clr = 5,
    Checkpoint = 6,
}

impl LogRecordType {
    /// Deserializes a byte into a `LogRecordType`.
    pub fn from_u8(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Commit),
            3 => Ok(Self::Abort),
            4 => Ok(Self::PageDelta),
            5 => Ok(Self::Clr),
            6 => Ok(Self::Checkpoint),
            _ => Err(Error::SerializationFailure(format!(
                "invalid log record type tag: {tag}"
            ))),
        }
    }

    /// Returns the raw byte discriminant.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Payload specific to each log record type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRecordBody {
    /// Transaction begin marker.
    Begin,
    /// Transaction commit marker carrying the durable allocated commit sequence.
    Commit { commit_seq: u64 },
    /// Transaction abort marker.
    Abort,
    /// Physical byte delta applied to a page, containing both before and after images.
    PageDelta {
        page_id: PageId,
        offset: u16,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    },
    /// Compensation Log Record generated during undo, carrying redo bytes and the next undo LSN.
    Clr {
        page_id: PageId,
        offset: u16,
        redo_image: Vec<u8>,
        undo_next_lsn: Lsn,
    },
    /// Checkpoint snapshot storing active transactions and their last logged LSNs.
    Checkpoint { active_txns: Vec<(TxnId, Lsn)> },
}

/// In-memory representation of an individual WAL log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub lsn: Lsn,
    pub prev_lsn: Lsn,
    pub txn_id: TxnId,
    pub record_type: LogRecordType,
    pub body: LogRecordBody,
}

impl LogRecord {
    /// Creates a new `Begin` log record.
    pub fn new_begin(lsn: Lsn, txn_id: TxnId) -> Self {
        Self {
            lsn,
            prev_lsn: NULL_LSN,
            txn_id,
            record_type: LogRecordType::Begin,
            body: LogRecordBody::Begin,
        }
    }

    /// Creates a new `Commit` log record with default zero sequence.
    pub fn new_commit(lsn: Lsn, prev_lsn: Lsn, txn_id: TxnId) -> Self {
        Self::new_commit_with_seq(lsn, prev_lsn, txn_id, 0)
    }

    /// Creates a new `Commit` log record with an explicit commit sequence number.
    pub fn new_commit_with_seq(lsn: Lsn, prev_lsn: Lsn, txn_id: TxnId, commit_seq: u64) -> Self {
        Self {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Commit,
            body: LogRecordBody::Commit { commit_seq },
        }
    }

    /// Creates a new `Abort` log record.
    pub fn new_abort(lsn: Lsn, prev_lsn: Lsn, txn_id: TxnId) -> Self {
        Self {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Abort,
            body: LogRecordBody::Abort,
        }
    }

    /// Creates a new `PageDelta` physical update log record.
    pub fn new_page_delta(
        lsn: Lsn,
        prev_lsn: Lsn,
        txn_id: TxnId,
        page_id: PageId,
        offset: u16,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    ) -> Self {
        Self {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::PageDelta,
            body: LogRecordBody::PageDelta {
                page_id,
                offset,
                before_image,
                after_image,
            },
        }
    }

    /// Creates a new Compensation Log Record (CLR).
    pub fn new_clr(
        lsn: Lsn,
        prev_lsn: Lsn,
        txn_id: TxnId,
        page_id: PageId,
        offset: u16,
        redo_image: Vec<u8>,
        undo_next_lsn: Lsn,
    ) -> Self {
        Self {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Clr,
            body: LogRecordBody::Clr {
                page_id,
                offset,
                redo_image,
                undo_next_lsn,
            },
        }
    }

    /// Creates a new `Checkpoint` log record.
    pub fn new_checkpoint(lsn: Lsn, active_txns: Vec<(TxnId, Lsn)>) -> Self {
        Self {
            lsn,
            prev_lsn: NULL_LSN,
            txn_id: NULL_TXN,
            record_type: LogRecordType::Checkpoint,
            body: LogRecordBody::Checkpoint { active_txns },
        }
    }

    /// Serializes this log record into a binary buffer including length prefix and CRC32.
    ///
    /// Layout:
    /// `[payload_len: u32] [lsn: u64] [prev_lsn: u64] [txn_id: u64] [record_type: u8] [body_bytes...] [crc32: u32]`
    pub fn serialize(&self) -> Vec<u8> {
        let mut content = Vec::with_capacity(64);
        content.extend_from_slice(&self.lsn.to_bytes());
        content.extend_from_slice(&self.prev_lsn.to_bytes());
        content.extend_from_slice(&self.txn_id.to_bytes());
        content.push(self.record_type.to_u8());

        match &self.body {
            LogRecordBody::Begin | LogRecordBody::Abort => {}
            LogRecordBody::Commit { commit_seq } => {
                content.extend_from_slice(&commit_seq.to_le_bytes());
            }
            LogRecordBody::PageDelta {
                page_id,
                offset,
                before_image,
                after_image,
            } => {
                content.extend_from_slice(&page_id.to_le_bytes());
                content.extend_from_slice(&offset.to_le_bytes());
                content.extend_from_slice(&(before_image.len() as u16).to_le_bytes());
                content.extend_from_slice(before_image);
                content.extend_from_slice(&(after_image.len() as u16).to_le_bytes());
                content.extend_from_slice(after_image);
            }
            LogRecordBody::Clr {
                page_id,
                offset,
                redo_image,
                undo_next_lsn,
            } => {
                content.extend_from_slice(&page_id.to_le_bytes());
                content.extend_from_slice(&offset.to_le_bytes());
                content.extend_from_slice(&undo_next_lsn.to_bytes());
                content.extend_from_slice(&(redo_image.len() as u16).to_le_bytes());
                content.extend_from_slice(redo_image);
            }
            LogRecordBody::Checkpoint { active_txns } => {
                content.extend_from_slice(&(active_txns.len() as u32).to_le_bytes());
                for (txn_id, last_lsn) in active_txns {
                    content.extend_from_slice(&txn_id.to_bytes());
                    content.extend_from_slice(&last_lsn.to_bytes());
                }
            }
        }

        let checksum = crc32(&content);
        let total_payload_len = (content.len() + 4) as u32;

        let mut output = Vec::with_capacity(4 + content.len() + 4);
        output.extend_from_slice(&total_payload_len.to_le_bytes());
        output.extend_from_slice(&content);
        output.extend_from_slice(&checksum.to_le_bytes());
        output
    }

    /// Deserializes a log record from a slice, returning `(LogRecord, bytes_consumed)` if valid.
    pub fn deserialize(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < 4 {
            return Err(Error::SerializationFailure(
                "insufficient bytes for log record length prefix".into(),
            ));
        }

        let payload_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if payload_len < 4 + 8 + 8 + 8 + 1 {
            return Err(Error::SerializationFailure(format!(
                "invalid log record payload length: {payload_len}"
            )));
        }

        if bytes.len() < 4 + payload_len {
            return Err(Error::SerializationFailure(format!(
                "truncated log record: expected {} bytes, got {}",
                4 + payload_len,
                bytes.len()
            )));
        }

        let content_end = 4 + payload_len - 4;
        let content = &bytes[4..content_end];
        let expected_checksum = u32::from_le_bytes([
            bytes[content_end],
            bytes[content_end + 1],
            bytes[content_end + 2],
            bytes[content_end + 3],
        ]);

        let actual_checksum = crc32(content);
        if actual_checksum != expected_checksum {
            return Err(Error::SerializationFailure(format!(
                "log record CRC32 mismatch: expected {expected_checksum:#010X}, found {actual_checksum:#010X}"
            )));
        }

        let lsn = Lsn::from_bytes(content[0..8].try_into().unwrap());
        let prev_lsn = Lsn::from_bytes(content[8..16].try_into().unwrap());
        let txn_id = TxnId::from_bytes(content[16..24].try_into().unwrap());
        let record_type = LogRecordType::from_u8(content[24])?;

        let body_bytes = &content[25..];
        let body = match record_type {
            LogRecordType::Begin => LogRecordBody::Begin,
            LogRecordType::Commit => {
                let commit_seq = if body_bytes.len() >= 8 {
                    u64::from_le_bytes(body_bytes[0..8].try_into().unwrap())
                } else {
                    0
                };
                LogRecordBody::Commit { commit_seq }
            }
            LogRecordType::Abort => LogRecordBody::Abort,
            LogRecordType::PageDelta => {
                if body_bytes.len() < 8 {
                    return Err(Error::SerializationFailure(
                        "insufficient bytes for PageDelta header".into(),
                    ));
                }
                let page_id = u32::from_le_bytes(body_bytes[0..4].try_into().unwrap());
                let offset = u16::from_le_bytes(body_bytes[4..6].try_into().unwrap());
                let before_len = u16::from_le_bytes(body_bytes[6..8].try_into().unwrap()) as usize;

                let mut pos = 8;
                if body_bytes.len() < pos + before_len + 2 {
                    return Err(Error::SerializationFailure(
                        "truncated PageDelta before image".into(),
                    ));
                }
                let before_image = body_bytes[pos..pos + before_len].to_vec();
                pos += before_len;

                let after_len =
                    u16::from_le_bytes(body_bytes[pos..pos + 2].try_into().unwrap()) as usize;
                pos += 2;
                if body_bytes.len() < pos + after_len {
                    return Err(Error::SerializationFailure(
                        "truncated PageDelta after image".into(),
                    ));
                }
                let after_image = body_bytes[pos..pos + after_len].to_vec();

                LogRecordBody::PageDelta {
                    page_id,
                    offset,
                    before_image,
                    after_image,
                }
            }
            LogRecordType::Clr => {
                if body_bytes.len() < 16 {
                    return Err(Error::SerializationFailure(
                        "insufficient bytes for CLR header".into(),
                    ));
                }
                let page_id = u32::from_le_bytes(body_bytes[0..4].try_into().unwrap());
                let offset = u16::from_le_bytes(body_bytes[4..6].try_into().unwrap());
                let undo_next_lsn = Lsn::from_bytes(body_bytes[6..14].try_into().unwrap());
                let redo_len = u16::from_le_bytes(body_bytes[14..16].try_into().unwrap()) as usize;

                if body_bytes.len() < 16 + redo_len {
                    return Err(Error::SerializationFailure(
                        "truncated CLR redo image".into(),
                    ));
                }
                let redo_image = body_bytes[16..16 + redo_len].to_vec();

                LogRecordBody::Clr {
                    page_id,
                    offset,
                    redo_image,
                    undo_next_lsn,
                }
            }
            LogRecordType::Checkpoint => {
                if body_bytes.len() < 4 {
                    return Err(Error::SerializationFailure(
                        "insufficient bytes for Checkpoint count".into(),
                    ));
                }
                let num_txns = u32::from_le_bytes(body_bytes[0..4].try_into().unwrap()) as usize;
                let mut pos = 4;
                let mut active_txns = Vec::with_capacity(num_txns);
                for _ in 0..num_txns {
                    if body_bytes.len() < pos + 16 {
                        return Err(Error::SerializationFailure(
                            "truncated Checkpoint active transaction list".into(),
                        ));
                    }
                    let tid = TxnId::from_bytes(body_bytes[pos..pos + 8].try_into().unwrap());
                    let lsn = Lsn::from_bytes(body_bytes[pos + 8..pos + 16].try_into().unwrap());
                    active_txns.push((tid, lsn));
                    pos += 16;
                }
                LogRecordBody::Checkpoint { active_txns }
            }
        };

        Ok((
            Self {
                lsn,
                prev_lsn,
                txn_id,
                record_type,
                body,
            },
            4 + payload_len,
        ))
    }
}

/// Standard IEEE 802.3 CRC32 calculation.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = ((crc & 1) != 0) as u32;
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!mask).wrapping_add(1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_record_begin_commit_abort_roundtrip() {
        let rec = LogRecord::new_begin(Lsn(1), TxnId(42));
        let bytes = rec.serialize();
        let (restored, consumed) = LogRecord::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(restored, rec);

        let rec_commit = LogRecord::new_commit(Lsn(2), Lsn(1), TxnId(42));
        let bytes = rec_commit.serialize();
        let (restored_commit, consumed) = LogRecord::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(restored_commit, rec_commit);
    }

    #[test]
    fn test_log_record_page_delta_roundtrip() {
        let rec = LogRecord::new_page_delta(
            Lsn(5),
            Lsn(1),
            TxnId(10),
            4,
            128,
            vec![1, 2, 3],
            vec![4, 5, 6, 7],
        );
        let bytes = rec.serialize();
        let (restored, consumed) = LogRecord::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(restored, rec);
    }

    #[test]
    fn test_log_record_clr_roundtrip() {
        let rec = LogRecord::new_clr(Lsn(6), Lsn(5), TxnId(10), 4, 128, vec![1, 2, 3], Lsn(1));
        let bytes = rec.serialize();
        let (restored, consumed) = LogRecord::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(restored, rec);
    }

    #[test]
    fn test_log_record_checkpoint_roundtrip() {
        let rec = LogRecord::new_checkpoint(Lsn(10), vec![(TxnId(1), Lsn(4)), (TxnId(2), Lsn(8))]);
        let bytes = rec.serialize();
        let (restored, consumed) = LogRecord::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(restored, rec);
    }

    #[test]
    fn test_crc_corruption_detection() {
        let rec = LogRecord::new_begin(Lsn(1), TxnId(42));
        let mut bytes = rec.serialize();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // Corrupt checksum
        let res = LogRecord::deserialize(&bytes);
        assert!(res.is_err());
    }
}
