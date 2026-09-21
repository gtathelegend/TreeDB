use crate::error::{Error, Result};
use crate::storage::page::{PageId, PAGE_SIZE};

/// Recognizable magic bytes for TreeDB database files.
pub const MAGIC_BYTES: &[u8; 8] = b"TREEDB01";

/// Format version 2 (with 16-byte page headers containing PageLSN).
pub const CURRENT_FORMAT_VERSION: u32 = 2;

/// Page ID for the Database Header.
pub const HEADER_PAGE_ID: PageId = 0;

/// Page ID for the initial Database Metadata/Catalog.
pub const DEFAULT_METADATA_PAGE_ID: PageId = 1;

/// In-memory representation of the Database Header stored on Page 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbHeader {
    /// Magic identifier ("TREEDB01").
    pub magic: [u8; 8],
    /// On-disk format version.
    pub format_version: u32,
    /// Configured page size in bytes.
    pub page_size: u32,
    /// Total number of allocated pages in the database file.
    pub page_count: u32,
    /// Root page ID of the primary B+ Tree (0 if uninitialized).
    pub root_page_id: PageId,
    /// Page ID of the metadata/catalog page.
    pub metadata_page_id: PageId,
}

impl Default for DbHeader {
    fn default() -> Self {
        Self::new()
    }
}

impl DbHeader {
    /// Creates a newly initialized `DbHeader` for a new database.
    pub fn new() -> Self {
        Self {
            magic: *MAGIC_BYTES,
            format_version: CURRENT_FORMAT_VERSION,
            page_size: PAGE_SIZE as u32,
            page_count: 2, // Page 0 (Header) + Page 1 (Metadata)
            root_page_id: 0,
            metadata_page_id: DEFAULT_METADATA_PAGE_ID,
        }
    }

    /// Serializes the header into a 4096-byte array for Page 0.
    pub fn serialize(&self) -> [u8; PAGE_SIZE] {
        let mut bytes = [0u8; PAGE_SIZE];
        bytes[0..8].copy_from_slice(&self.magic);
        bytes[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.page_count.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.root_page_id.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.metadata_page_id.to_le_bytes());
        bytes
    }

    /// Deserializes a 4096-byte page buffer into a `DbHeader`, validating header invariants.
    pub fn deserialize(bytes: &[u8; PAGE_SIZE]) -> Result<Self> {
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&bytes[0..8]);
        if &magic != MAGIC_BYTES {
            return Err(Error::InvalidDatabase(format!(
                "invalid magic bytes: expected {MAGIC_BYTES:?}, found {magic:?}"
            )));
        }

        let format_version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if format_version != CURRENT_FORMAT_VERSION {
            return Err(Error::UnsupportedFormatVersion {
                found: format_version,
                supported: CURRENT_FORMAT_VERSION,
            });
        }

        let page_size = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::InvalidDatabase(format!(
                "unsupported page size in header: found {page_size}, expected {PAGE_SIZE}"
            )));
        }

        let page_count = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let root_page_id = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let metadata_page_id = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);

        Ok(Self {
            magic,
            format_version,
            page_size,
            page_count,
            root_page_id,
            metadata_page_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let mut header = DbHeader::new();
        header.page_count = 10;
        header.root_page_id = 4;

        let bytes = header.serialize();
        let restored = DbHeader::deserialize(&bytes).expect("deserialization should succeed");
        assert_eq!(restored, header);
    }

    #[test]
    fn test_invalid_magic() {
        let mut bytes = DbHeader::new().serialize();
        bytes[0..8].copy_from_slice(b"BADMAGIC");
        let result = DbHeader::deserialize(&bytes);
        assert!(matches!(result, Err(Error::InvalidDatabase(_))));
    }

    #[test]
    fn test_unsupported_version() {
        let mut bytes = DbHeader::new().serialize();
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes()); // Old version 1
        let result = DbHeader::deserialize(&bytes);
        assert!(matches!(
            result,
            Err(Error::UnsupportedFormatVersion {
                found: 1,
                supported: 2
            })
        ));
    }
}
