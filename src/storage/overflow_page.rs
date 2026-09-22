use crate::error::{Error, Result};
use crate::storage::page::{Page, PageId, PageType, PAGE_PAYLOAD_SIZE};

/// Sentinel indicating that this is the final page in an overflow chain.
pub const NO_OVERFLOW_PAGE: PageId = 0;

/// Header size in bytes of the overflow page within the page payload.
///
/// Layout: `next_page_id` (4B) + `bytes_used` (2B) + `reserved` (2B) = 8 bytes.
pub const OVERFLOW_HEADER_SIZE: usize = 8;

/// Usable payload capacity in bytes for a single overflow page.
///
/// Derived as: `PAGE_PAYLOAD_SIZE (4080) - OVERFLOW_HEADER_SIZE (8) = 4072 bytes`.
pub const OVERFLOW_PAYLOAD_SIZE: usize = PAGE_PAYLOAD_SIZE - OVERFLOW_HEADER_SIZE;

/// In-memory representation of an Overflow Page (`PageType::Overflow`).
///
/// # Overflow Page Layout (Payload bytes 16..4096 of standard Page)
/// ```text
/// +---------------------------------------------------------------+
/// | Standard Page Header (16 bytes): [page_id: u32 | type=6 | ...] |
/// +---------------------------------------------------------------+
/// | Overflow Header (8 bytes):                                    |
/// |   next_page_id: u32 (4B) | bytes_used: u16 (2B) | reserved(2B)|
/// +---------------------------------------------------------------+
/// | Usable Overflow Payload Data (up to 4072 bytes)               |
/// +---------------------------------------------------------------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowPage {
    /// Next page ID in the singly-linked overflow chain, or `NO_OVERFLOW_PAGE` (0).
    pub next_page_id: PageId,
    /// Number of valid data bytes stored in this overflow page (0..=4072).
    pub bytes_used: u16,
    /// Buffer holding up to `OVERFLOW_PAYLOAD_SIZE` (4072) data bytes.
    data: [u8; OVERFLOW_PAYLOAD_SIZE],
}

impl Default for OverflowPage {
    fn default() -> Self {
        Self::empty()
    }
}

impl OverflowPage {
    /// Creates an empty `OverflowPage`.
    pub const fn empty() -> Self {
        Self {
            next_page_id: NO_OVERFLOW_PAGE,
            bytes_used: 0,
            data: [0u8; OVERFLOW_PAYLOAD_SIZE],
        }
    }

    /// Creates a new `OverflowPage` populated with slice data and the next link pointer.
    pub fn new(next_page_id: PageId, data: &[u8]) -> Result<Self> {
        let len = data.len();
        if len > OVERFLOW_PAYLOAD_SIZE {
            return Err(Error::ValueTooLarge {
                size: len,
                limit: OVERFLOW_PAYLOAD_SIZE,
            });
        }

        let mut buf = [0u8; OVERFLOW_PAYLOAD_SIZE];
        buf[..len].copy_from_slice(data);

        Ok(Self {
            next_page_id,
            bytes_used: len as u16,
            data: buf,
        })
    }

    /// Returns the ID of the next overflow page in the chain, or `NO_OVERFLOW_PAGE`.
    pub const fn next_page_id(&self) -> PageId {
        self.next_page_id
    }

    /// Sets the ID of the next overflow page in the chain.
    pub fn set_next_page_id(&mut self, next: PageId) {
        self.next_page_id = next;
    }

    /// Returns the number of valid payload bytes stored in this page.
    pub const fn bytes_used(&self) -> usize {
        self.bytes_used as usize
    }

    /// Returns an immutable slice of the valid payload bytes stored in this page.
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.bytes_used as usize]
    }

    /// Returns a mutable slice of the valid payload bytes stored in this page.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        let used = self.bytes_used as usize;
        &mut self.data[..used]
    }

    /// Sets the valid payload bytes stored in this page.
    pub fn set_payload(&mut self, data: &[u8]) -> Result<()> {
        let len = data.len();
        if len > OVERFLOW_PAYLOAD_SIZE {
            return Err(Error::ValueTooLarge {
                size: len,
                limit: OVERFLOW_PAYLOAD_SIZE,
            });
        }
        self.data[..len].copy_from_slice(data);
        self.bytes_used = len as u16;
        Ok(())
    }

    /// Validates the structural integrity of the overflow page.
    pub fn validate(&self) -> Result<()> {
        if self.bytes_used as usize > OVERFLOW_PAYLOAD_SIZE {
            return Err(Error::OverflowCorruption(format!(
                "bytes_used ({}) exceeds maximum capacity ({OVERFLOW_PAYLOAD_SIZE})",
                self.bytes_used
            )));
        }
        Ok(())
    }

    /// Serializes this `OverflowPage` into a raw `Page` buffer.
    pub fn serialize(&self, page: &mut Page) -> Result<()> {
        self.validate()?;

        page.set_page_type(PageType::Overflow);
        let payload = page.payload_mut();

        // 1. Write next_page_id (4B, LE)
        payload[0..4].copy_from_slice(&self.next_page_id.to_le_bytes());

        // 2. Write bytes_used (2B, LE)
        payload[4..6].copy_from_slice(&self.bytes_used.to_le_bytes());

        // 3. Write reserved (2B, LE)
        payload[6..8].copy_from_slice(&0u16.to_le_bytes());

        // 4. Write payload data
        let used = self.bytes_used as usize;
        payload[OVERFLOW_HEADER_SIZE..OVERFLOW_HEADER_SIZE + used]
            .copy_from_slice(&self.data[..used]);

        // Zero out unused trailing payload space
        payload[OVERFLOW_HEADER_SIZE + used..PAGE_PAYLOAD_SIZE].fill(0);

        Ok(())
    }

    /// Deserializes an `OverflowPage` from a raw `Page` buffer.
    pub fn deserialize(page: &Page) -> Result<Self> {
        if page.page_type() != PageType::Overflow {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "expected Overflow page type tag",
            });
        }

        let payload = page.payload();

        let next_page_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let bytes_used = u16::from_le_bytes([payload[4], payload[5]]);

        if bytes_used as usize > OVERFLOW_PAYLOAD_SIZE {
            return Err(Error::PageCorruption {
                page_id: page.id(),
                reason: "overflow bytes_used exceeds maximum allowable page payload capacity",
            });
        }

        let mut data = [0u8; OVERFLOW_PAYLOAD_SIZE];
        let used = bytes_used as usize;
        data[..used].copy_from_slice(&payload[OVERFLOW_HEADER_SIZE..OVERFLOW_HEADER_SIZE + used]);

        let overflow_page = Self {
            next_page_id,
            bytes_used,
            data,
        };

        overflow_page.validate()?;
        Ok(overflow_page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_page_roundtrip() {
        let test_data = b"TreeDB Multi-Page Overflow Payload";
        let op = OverflowPage::new(42, test_data).expect("construct");
        assert_eq!(op.next_page_id(), 42);
        assert_eq!(op.bytes_used(), test_data.len());
        assert_eq!(op.payload(), test_data);

        let mut page = Page::new(100, PageType::Overflow);
        op.serialize(&mut page).expect("serialize");

        let restored = OverflowPage::deserialize(&page).expect("deserialize");
        assert_eq!(restored, op);
        assert_eq!(restored.next_page_id(), 42);
        assert_eq!(restored.bytes_used(), test_data.len());
        assert_eq!(restored.payload(), test_data);
    }

    #[test]
    fn test_overflow_page_max_capacity() {
        let max_data = vec![0xAA; OVERFLOW_PAYLOAD_SIZE];
        let op = OverflowPage::new(NO_OVERFLOW_PAGE, &max_data).expect("construct max");
        assert_eq!(op.bytes_used(), OVERFLOW_PAYLOAD_SIZE);

        let mut page = Page::new(101, PageType::Overflow);
        op.serialize(&mut page).expect("serialize");

        let restored = OverflowPage::deserialize(&page).expect("deserialize");
        assert_eq!(restored.payload(), &max_data[..]);

        // Exceeding capacity fails
        let over_data = vec![0xBB; OVERFLOW_PAYLOAD_SIZE + 1];
        let err = OverflowPage::new(NO_OVERFLOW_PAGE, &over_data).unwrap_err();
        assert!(matches!(err, Error::ValueTooLarge { .. }));
    }

    #[test]
    fn test_overflow_page_invalid_type_rejection() {
        let page = Page::new(102, PageType::Record);
        let err = OverflowPage::deserialize(&page).unwrap_err();
        assert!(matches!(err, Error::PageCorruption { .. }));
    }
}
