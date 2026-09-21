use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::storage::metadata::{is_reserved_page, NO_FREE_PAGE};
use crate::storage::page::{Page, PageId, PageType};
use crate::storage::page_manager::PageManager;

/// Helper functions for reading and writing the `next_free` pointer within a free page.
pub struct FreePageLayout;

impl FreePageLayout {
    /// Reads the `next_free` page pointer from a `PageType::Free` page payload.
    pub fn read_next_free(page: &Page) -> PageId {
        let payload = page.payload();
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
    }

    /// Writes the `next_free` page pointer into a `PageType::Free` page payload.
    pub fn write_next_free(page: &mut Page, next_free: PageId) {
        page.set_page_type(PageType::Free);
        page.payload_mut()[0..4].copy_from_slice(&next_free.to_le_bytes());
    }
}

/// Manages persistent free-page recycling and freelist integrity.
///
/// Free pages form a singly-linked list on disk with `DbHeader::freelist_head` pointing
/// to the most recently freed page (LIFO order).
#[derive(Debug, Default)]
pub struct FreePageManager;

impl FreePageManager {
    /// Checks if a page ID is currently in the freelist.
    pub fn is_free(page_manager: &mut PageManager, page_id: PageId) -> Result<bool> {
        let mut curr = page_manager.header().freelist_head;
        let total_pages = page_manager.page_count();
        let mut visited = HashSet::new();

        while curr != NO_FREE_PAGE {
            if curr == page_id {
                return Ok(true);
            }
            if curr >= total_pages || is_reserved_page(curr) || !visited.insert(curr) {
                break;
            }
            let page = page_manager.read_page(curr)?;
            if page.page_type() != PageType::Free {
                break;
            }
            curr = FreePageLayout::read_next_free(&page);
        }

        Ok(false)
    }

    /// Reclaims an unlinked page into the persistent freelist.
    ///
    /// # Errors
    /// * `Error::CannotFreeReservedPage` if attempting to free Page 0 (Header) or Page 1 (Metadata).
    /// * `Error::InvalidPage` if `page_id >= page_count`.
    /// * `Error::PageAlreadyFree` if the target page is already in the freelist.
    pub fn free_page(page_manager: &mut PageManager, page_id: PageId) -> Result<()> {
        if is_reserved_page(page_id) {
            return Err(Error::CannotFreeReservedPage(page_id));
        }

        if page_id >= page_manager.page_count() {
            return Err(Error::InvalidPage(page_id));
        }

        if Self::is_free(page_manager, page_id)? {
            return Err(Error::PageAlreadyFree(page_id));
        }

        let curr_head = page_manager.header().freelist_head;

        let mut free_page = Page::new(page_id, PageType::Free);
        FreePageLayout::write_next_free(&mut free_page, curr_head);
        page_manager.write_page(&free_page)?;

        page_manager.header_mut().freelist_head = page_id;
        page_manager.header_mut().free_page_count += 1;
        page_manager.write_header_to_disk()?;

        Ok(())
    }

    /// Reuses a free page from the freelist head if available.
    ///
    /// Returns `Ok(Some(page_id))` if a recyclable page was popped, or `Ok(None)` if the freelist is empty.
    pub fn allocate_page(page_manager: &mut PageManager) -> Result<Option<PageId>> {
        let curr_head = page_manager.header().freelist_head;
        if curr_head == NO_FREE_PAGE {
            return Ok(None);
        }

        if is_reserved_page(curr_head) || curr_head >= page_manager.page_count() {
            return Err(Error::FreelistCorruption(format!(
                "invalid freelist head page ID {curr_head}"
            )));
        }

        let free_page = page_manager.read_page(curr_head)?;
        if free_page.page_type() != PageType::Free {
            return Err(Error::FreelistCorruption(format!(
                "freelist head page {curr_head} has unexpected type {:?}",
                free_page.page_type()
            )));
        }

        let next_free = FreePageLayout::read_next_free(&free_page);
        if next_free != NO_FREE_PAGE
            && (is_reserved_page(next_free) || next_free >= page_manager.page_count())
        {
            return Err(Error::FreelistCorruption(format!(
                "freelist page {curr_head} points to invalid next_free {next_free}"
            )));
        }

        page_manager.header_mut().freelist_head = next_free;
        let count = page_manager.header().free_page_count;
        page_manager.header_mut().free_page_count = count.saturating_sub(1);
        page_manager.write_header_to_disk()?;

        Ok(Some(curr_head))
    }

    /// Validates the structural integrity of the entire persistent freelist.
    ///
    /// Checks:
    /// 1. No reserved pages in the freelist.
    /// 2. No out-of-bounds page IDs.
    /// 3. No cycles (no duplicate page IDs).
    /// 4. Every linked page is `PageType::Free`.
    /// 5. Count of traversed pages matches `DbHeader::free_page_count`.
    /// 6. Active root and metadata pages are not in the freelist.
    pub fn validate(page_manager: &mut PageManager) -> Result<()> {
        let mut curr = page_manager.header().freelist_head;
        let expected_count = page_manager.header().free_page_count as usize;
        let mut visited = HashSet::new();
        let total_pages = page_manager.page_count();
        let root_page_id = page_manager.header().root_page_id;

        while curr != NO_FREE_PAGE {
            if is_reserved_page(curr) {
                return Err(Error::FreelistCorruption(format!(
                    "freelist contains reserved page {curr}"
                )));
            }
            if curr >= total_pages {
                return Err(Error::FreelistCorruption(format!(
                    "freelist contains out-of-bounds page {curr} (total pages: {total_pages})"
                )));
            }
            if !visited.insert(curr) {
                return Err(Error::FreelistCorruption(format!(
                    "cycle detected in freelist at page {curr}"
                )));
            }
            if root_page_id != 0 && curr == root_page_id {
                return Err(Error::FreelistCorruption(format!(
                    "freelist contains active B+ Tree root page {curr}"
                )));
            }

            let page = page_manager.read_page(curr)?;
            if page.page_type() != PageType::Free {
                return Err(Error::FreelistCorruption(format!(
                    "freelist page {curr} is marked as {:?} instead of PageType::Free",
                    page.page_type()
                )));
            }

            curr = FreePageLayout::read_next_free(&page);
        }

        if visited.len() != expected_count {
            return Err(Error::FreelistCorruption(format!(
                "freelist count mismatch: header recorded {expected_count}, but traversed {}",
                visited.len()
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_freelist_basic_lifecycle() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.treedb");
        let mut pm = PageManager::create(&db_path).unwrap();

        // Allocate pages 2, 3, 4 and write them as user pages
        let p2 = pm.allocate_page().unwrap();
        let p3 = pm.allocate_page().unwrap();
        let p4 = pm.allocate_page().unwrap();
        assert_eq!(p2, 2);
        assert_eq!(p3, 3);
        assert_eq!(p4, 4);
        assert_eq!(pm.header().freelist_head, NO_FREE_PAGE);
        assert_eq!(pm.header().free_page_count, 0);

        pm.write_page(&Page::new(p2, PageType::Leaf)).unwrap();
        pm.write_page(&Page::new(p3, PageType::Record)).unwrap();
        pm.write_page(&Page::new(p4, PageType::Internal)).unwrap();

        // Free page 3
        FreePageManager::free_page(&mut pm, p3).unwrap();

        assert_eq!(pm.header().freelist_head, 3);
        assert_eq!(pm.header().free_page_count, 1);
        FreePageManager::validate(&mut pm).unwrap();

        // Double free page 3 should fail
        let double_free = FreePageManager::free_page(&mut pm, p3);
        assert!(matches!(double_free, Err(Error::PageAlreadyFree(3))));

        // Free reserved pages should fail
        assert!(matches!(
            FreePageManager::free_page(&mut pm, 0),
            Err(Error::CannotFreeReservedPage(0))
        ));
        assert!(matches!(
            FreePageManager::free_page(&mut pm, 1),
            Err(Error::CannotFreeReservedPage(1))
        ));

        // Free page 4
        FreePageManager::free_page(&mut pm, p4).unwrap();
        assert_eq!(pm.header().freelist_head, 4);
        assert_eq!(pm.header().free_page_count, 2);
        FreePageManager::validate(&mut pm).unwrap();

        // Allocate (reuse page 4 first - LIFO)
        let reused1 = FreePageManager::allocate_page(&mut pm).unwrap();
        assert_eq!(reused1, Some(4));
        assert_eq!(pm.header().freelist_head, 3);
        assert_eq!(pm.header().free_page_count, 1);

        // Allocate (reuse page 3)
        let reused2 = FreePageManager::allocate_page(&mut pm).unwrap();
        assert_eq!(reused2, Some(3));
        assert_eq!(pm.header().freelist_head, NO_FREE_PAGE);
        assert_eq!(pm.header().free_page_count, 0);

        // Freelist is empty now -> returns None
        let empty = FreePageManager::allocate_page(&mut pm).unwrap();
        assert_eq!(empty, None);
    }
}
