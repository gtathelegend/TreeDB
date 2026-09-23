use crate::error::{Error, Result};
use crate::wal::lsn::{Lsn, NULL_LSN};

/// Size of each page in bytes (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// Page identifier.
pub type PageId = u32;

/// Standard key type for index structures.
pub type Key = u64;

/// Standard page header size (16 bytes).
///
/// Layout:
/// - `0..4`: `page_id: u32` (little-endian)
/// - `4`: `page_type: u8`
/// - `5`: `flags: u8`
/// - `6..8`: `reserved: u16`
/// - `8..16`: `page_lsn: u64` (little-endian)
pub const PAGE_HEADER_SIZE: usize = 16;

/// Available payload capacity in a standard page (4080 bytes).
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// Identifies the role and layout of a database page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Metadata = 1,
    Internal = 2,
    Leaf = 3,
    Record = 4,
    Free = 5,
    Overflow = 6,
    Catalog = 7,
}

impl PageType {
    /// Converts a raw byte into a `PageType`, returning an error if unrecognized.
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(PageType::Metadata),
            2 => Ok(PageType::Internal),
            3 => Ok(PageType::Leaf),
            4 => Ok(PageType::Record),
            5 => Ok(PageType::Free),
            6 => Ok(PageType::Overflow),
            7 => Ok(PageType::Catalog),
            _ => Err(Error::SerializationFailure(format!(
                "invalid page type tag: {value}"
            ))),
        }
    }

    /// Converts this `PageType` into its persistent byte representation.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

/// In-memory representation of an individual 4096-byte database page.
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

impl Page {
    /// Creates a new page initialized with zeroes and standard header values.
    pub fn new(id: PageId, page_type: PageType) -> Self {
        let mut page = Self {
            data: [0u8; PAGE_SIZE],
        };
        page.set_id(id);
        page.set_page_type(page_type);
        page.set_lsn(NULL_LSN);
        page
    }

    /// Creates a Page directly from raw bytes, verifying structural correctness.
    pub fn from_bytes(data: [u8; PAGE_SIZE]) -> Result<Self> {
        let page = Self { data };
        page.validate()?;
        Ok(page)
    }

    /// Creates a raw unvalidated Page from byte array (e.g. for header page).
    pub const fn from_raw(data: [u8; PAGE_SIZE]) -> Self {
        Self { data }
    }

    /// Validates structural integrity of the page.
    pub fn validate(&self) -> Result<()> {
        let page_type_tag = self.data[4];
        PageType::from_u8(page_type_tag).map_err(|_| Error::PageCorruption {
            page_id: self.id(),
            reason: "invalid page type discriminant in header",
        })?;
        Ok(())
    }

    /// Returns the page ID from the page header.
    pub fn id(&self) -> PageId {
        u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])
    }

    /// Sets the page ID in the page header.
    pub fn set_id(&mut self, id: PageId) {
        let bytes = id.to_le_bytes();
        self.data[0..4].copy_from_slice(&bytes);
    }

    /// Returns the `PageType` from the page header.
    pub fn page_type(&self) -> PageType {
        PageType::from_u8(self.data[4]).unwrap_or(PageType::Free)
    }

    /// Sets the `PageType` in the page header.
    pub fn set_page_type(&mut self, page_type: PageType) {
        self.data[4] = page_type.to_u8();
    }

    /// Returns the page flags byte.
    pub fn flags(&self) -> u8 {
        self.data[5]
    }

    /// Sets the page flags byte.
    pub fn set_flags(&mut self, flags: u8) {
        self.data[5] = flags;
    }

    /// Returns the `page_lsn` from the page header.
    pub fn lsn(&self) -> Lsn {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data[8..16]);
        Lsn::from_bytes(bytes)
    }

    /// Sets the `page_lsn` in the page header.
    pub fn set_lsn(&mut self, lsn: Lsn) {
        self.data[8..16].copy_from_slice(&lsn.to_bytes());
    }

    /// Returns an immutable reference to the full 4096-byte buffer.
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    /// Returns a mutable reference to the full 4096-byte buffer.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.data
    }

    /// Returns an immutable slice of the page payload (bytes after header).
    pub fn payload(&self) -> &[u8] {
        &self.data[PAGE_HEADER_SIZE..]
    }

    /// Returns a mutable slice of the page payload (bytes after header).
    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.data[PAGE_HEADER_SIZE..]
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("id", &self.id())
            .field("page_type", &self.page_type())
            .field("flags", &self.flags())
            .field("lsn", &self.lsn())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_header_roundtrip() {
        let mut page = Page::new(42, PageType::Leaf);
        assert_eq!(page.id(), 42);
        assert_eq!(page.page_type(), PageType::Leaf);
        assert_eq!(page.flags(), 0);
        assert_eq!(page.lsn(), NULL_LSN);

        page.set_flags(0b0000_0001);
        assert_eq!(page.flags(), 1);

        page.set_lsn(Lsn::new(999));
        assert_eq!(page.lsn(), Lsn::new(999));

        page.payload_mut()[0..4].copy_from_slice(b"test");
        assert_eq!(&page.payload()[0..4], b"test");

        let raw = *page.as_bytes();
        let restored = Page::from_bytes(raw).expect("should validate");
        assert_eq!(restored.id(), 42);
        assert_eq!(restored.page_type(), PageType::Leaf);
        assert_eq!(restored.flags(), 1);
        assert_eq!(restored.lsn(), Lsn::new(999));
        assert_eq!(&restored.payload()[0..4], b"test");
    }

    #[test]
    fn test_invalid_page_type_validation() {
        let mut raw = [0u8; PAGE_SIZE];
        raw[4] = 99; // Invalid tag
        let result = Page::from_bytes(raw);
        assert!(result.is_err());
    }
}
