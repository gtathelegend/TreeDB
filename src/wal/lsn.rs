use std::fmt;

/// Monotonically increasing Log Sequence Number identifying a position in the WAL stream.
///
/// LSN 0 (`NULL_LSN`) is reserved to denote null / unassigned / invalid LSN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Lsn(pub u64);

/// Special null/uninitialized LSN constant.
pub const NULL_LSN: Lsn = Lsn(0);

/// Alias for `NULL_LSN`.
pub const INVALID_LSN: Lsn = Lsn(0);

impl Lsn {
    /// Returns a new `Lsn` wrapping the provided raw 64-bit integer.
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Returns `true` if this is a valid non-null LSN.
    pub const fn is_valid(&self) -> bool {
        self.0 != 0
    }

    /// Returns the raw integer value of this LSN.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Serializes this LSN into 8 little-endian bytes.
    pub const fn to_bytes(&self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Deserializes an LSN from 8 little-endian bytes.
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_le_bytes(bytes))
    }

    /// Returns the next sequential LSN.
    pub const fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LSN({})", self.0)
    }
}

/// Monotonically allocated Transaction Identifier.
///
/// `TxnId(0)` is reserved as `NULL_TXN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TxnId(pub u64);

/// Special null/uninitialized transaction ID constant.
pub const NULL_TXN: TxnId = TxnId(0);

impl TxnId {
    /// Returns a new `TxnId` wrapping the provided raw 64-bit integer.
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Returns `true` if this is a valid non-null transaction identifier.
    pub const fn is_valid(&self) -> bool {
        self.0 != 0
    }

    /// Returns the raw integer value of this transaction ID.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Serializes this `TxnId` into 8 little-endian bytes.
    pub const fn to_bytes(&self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Deserializes a `TxnId` from 8 little-endian bytes.
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_le_bytes(bytes))
    }

    /// Returns the next sequential transaction ID.
    pub const fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TXN({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsn_ordering_and_serialization() {
        let lsn1 = Lsn::new(100);
        let lsn2 = Lsn::new(200);
        assert!(lsn1 < lsn2);
        assert!(lsn1.is_valid());
        assert!(!NULL_LSN.is_valid());

        let bytes = lsn1.to_bytes();
        let restored = Lsn::from_bytes(bytes);
        assert_eq!(restored, lsn1);
        assert_eq!(lsn1.next(), Lsn::new(101));
    }

    #[test]
    fn test_txn_id_ordering_and_serialization() {
        let t1 = TxnId::new(1);
        let t2 = TxnId::new(2);
        assert!(t1 < t2);
        assert!(t1.is_valid());
        assert!(!NULL_TXN.is_valid());

        let bytes = t1.to_bytes();
        let restored = TxnId::from_bytes(bytes);
        assert_eq!(restored, t1);
        assert_eq!(t1.next(), TxnId::new(2));
    }
}
