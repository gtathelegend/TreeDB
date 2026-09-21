use std::path::Path;
use tempfile::tempdir;

use treedb::error::Error;
use treedb::index::bplus_tree::BPlusTree;
use treedb::index::node::RecordId;
use treedb::storage::buffer_pool::BufferPool;
use treedb::storage::freelist::FreePageManager;
use treedb::storage::metadata::{is_reserved_page, NO_FREE_PAGE};
use treedb::storage::page::{Page, PageType};
use treedb::storage::page_manager::PageManager;

fn create_test_pool(capacity: usize, path: &Path) -> BufferPool {
    let pm = PageManager::create(path).expect("create pm");
    BufferPool::new(capacity, pm).expect("create pool")
}

#[test]
fn test_reserved_pages_cannot_be_freed() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_reserved.treedb");
    let mut pool = create_test_pool(5, &db_path);

    // Page 0 is Database Header
    assert!(is_reserved_page(0));
    let err0 = pool.free_page(0).unwrap_err();
    assert!(matches!(err0, Error::CannotFreeReservedPage(0)));

    // Page 1 is Metadata Page
    assert!(is_reserved_page(1));
    let err1 = pool.free_page(1).unwrap_err();
    assert!(matches!(err1, Error::CannotFreeReservedPage(1)));
}

#[test]
fn test_invalid_and_out_of_bounds_page_free_rejected() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_oob.treedb");
    let mut pool = create_test_pool(5, &db_path);

    // Initial database has page_count = 2 (Pages 0 and 1)
    let err = pool.free_page(2).unwrap_err();
    assert!(matches!(err, Error::InvalidPage(2)));

    let err_large = pool.free_page(9999).unwrap_err();
    assert!(matches!(err_large, Error::InvalidPage(9999)));
}

#[test]
fn test_double_free_rejected() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_double_free.treedb");
    let mut pool = create_test_pool(5, &db_path);

    let p2 = pool.allocate_page(PageType::Record).unwrap();
    pool.unpin_page(p2, true).unwrap();

    // First free succeeds
    pool.free_page(p2).expect("first free should succeed");

    // Second free fails with PageAlreadyFree
    let double_free_err = pool.free_page(p2).unwrap_err();
    assert!(matches!(double_free_err, Error::PageAlreadyFree(2)));
}

#[test]
fn test_basic_page_reuse_lifo_order() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_reuse.treedb");
    let mut pool = create_test_pool(5, &db_path);

    let p2 = pool.allocate_page(PageType::Record).unwrap();
    let p3 = pool.allocate_page(PageType::Record).unwrap();
    let p4 = pool.allocate_page(PageType::Record).unwrap();
    pool.unpin_page(p2, false).unwrap();
    pool.unpin_page(p3, false).unwrap();
    pool.unpin_page(p4, false).unwrap();

    assert_eq!(p2, 2);
    assert_eq!(p3, 3);
    assert_eq!(p4, 4);
    assert_eq!(pool.page_count(), 5);

    // Free p3 then p4
    pool.free_page(p3).unwrap();
    assert_eq!(pool.page_manager().header().freelist_head, 3);
    assert_eq!(pool.page_manager().header().free_page_count, 1);

    pool.free_page(p4).unwrap();
    assert_eq!(pool.page_manager().header().freelist_head, 4);
    assert_eq!(pool.page_manager().header().free_page_count, 2);

    // Allocate -> should reuse p4 first (LIFO)
    let reused1 = pool.allocate_page(PageType::Leaf).unwrap();
    assert_eq!(reused1, 4);
    assert_eq!(pool.page_manager().header().freelist_head, 3);
    assert_eq!(pool.page_manager().header().free_page_count, 1);
    pool.unpin_page(reused1, false).unwrap();

    // Allocate -> should reuse p3
    let reused2 = pool.allocate_page(PageType::Internal).unwrap();
    assert_eq!(reused2, 3);
    assert_eq!(pool.page_manager().header().freelist_head, NO_FREE_PAGE);
    assert_eq!(pool.page_manager().header().free_page_count, 0);
    pool.unpin_page(reused2, false).unwrap();

    // File size has not grown beyond 5 pages
    assert_eq!(pool.page_count(), 5);
}

#[test]
fn test_freelist_exhaustion_and_append_fallback() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_exhaust.treedb");
    let mut pool = create_test_pool(5, &db_path);

    let p2 = pool.allocate_page(PageType::Record).unwrap();
    pool.unpin_page(p2, false).unwrap();
    assert_eq!(pool.page_count(), 3);

    pool.free_page(p2).unwrap();
    assert_eq!(pool.page_manager().header().free_page_count, 1);

    // First allocate reuses p2
    let alloc1 = pool.allocate_page(PageType::Leaf).unwrap();
    assert_eq!(alloc1, 2);
    pool.unpin_page(alloc1, false).unwrap();

    // Second allocate falls back to appending page 3
    let alloc2 = pool.allocate_page(PageType::Leaf).unwrap();
    assert_eq!(alloc2, 3);
    assert_eq!(pool.page_count(), 4);
    pool.unpin_page(alloc2, false).unwrap();
}

#[test]
fn test_freelist_persistence_across_close_reopen() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_persist.treedb");

    {
        let mut pool = create_test_pool(5, &db_path);
        let p2 = pool.allocate_page(PageType::Record).unwrap();
        let p3 = pool.allocate_page(PageType::Record).unwrap();
        let p4 = pool.allocate_page(PageType::Record).unwrap();
        pool.unpin_page(p2, false).unwrap();
        pool.unpin_page(p3, false).unwrap();
        pool.unpin_page(p4, false).unwrap();

        pool.free_page(p3).unwrap();
        pool.free_page(p4).unwrap();
        pool.sync().unwrap();
    }

    // Reopen database
    {
        let pm = PageManager::open(&db_path).unwrap();
        assert_eq!(pm.header().freelist_head, 4);
        assert_eq!(pm.header().free_page_count, 2);

        let mut pool = BufferPool::new(5, pm).unwrap();
        pool.validate_freelist().expect("freelist valid on reopen");

        let reused1 = pool.allocate_page(PageType::Leaf).unwrap();
        assert_eq!(reused1, 4);
        pool.unpin_page(reused1, false).unwrap();

        let reused2 = pool.allocate_page(PageType::Record).unwrap();
        assert_eq!(reused2, 3);
        pool.unpin_page(reused2, false).unwrap();

        let appended = pool.allocate_page(PageType::Record).unwrap();
        assert_eq!(appended, 5);
        pool.unpin_page(appended, false).unwrap();
    }
}

#[test]
fn test_reused_page_reinitialization_no_leak() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_reinit.treedb");
    let mut pool = create_test_pool(5, &db_path);

    // 1. Allocate page 2 as Record and fill with dirty garbage bytes
    let p2 = pool.allocate_page(PageType::Record).unwrap();
    {
        let page = pool.get_page_mut(p2).unwrap();
        page.payload_mut().fill(0xAA);
    }
    pool.unpin_page(p2, true).unwrap();
    pool.flush_all().unwrap();

    // 2. Free page 2
    pool.free_page(p2).unwrap();

    // 3. Reallocate page 2 as Leaf
    let reused = pool.allocate_page(PageType::Leaf).unwrap();
    assert_eq!(reused, p2);

    // 4. Verify no old bytes leaked: header is Leaf and payload is all zeroes
    let page = pool.get_page(reused).unwrap();
    assert_eq!(page.page_type(), PageType::Leaf);
    assert_eq!(page.id(), 2);
    assert_eq!(page.payload(), &[0u8; 4080][..]);
    pool.unpin_page(reused, false).unwrap();
}

#[test]
fn test_database_growth_bounded_with_recycling() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_bounded.treedb");
    let mut pool = create_test_pool(10, &db_path);

    // Allocate 10 pages (pages 2..12)
    let mut pages = Vec::new();
    for _ in 0..10 {
        let p = pool.allocate_page(PageType::Record).unwrap();
        pages.push(p);
        pool.unpin_page(p, false).unwrap();
    }
    assert_eq!(pool.page_count(), 12);

    // Free all 10 pages
    for &p in &pages {
        pool.free_page(p).unwrap();
    }
    assert_eq!(pool.page_manager().header().free_page_count, 10);
    pool.validate_freelist().expect("freelist valid");

    // Allocate 10 pages again
    let mut reused_pages = Vec::new();
    for _ in 0..10 {
        let p = pool.allocate_page(PageType::Leaf).unwrap();
        reused_pages.push(p);
        pool.unpin_page(p, false).unwrap();
    }

    // Database file did not grow: page_count is still 12!
    assert_eq!(pool.page_count(), 12);
    assert_eq!(pool.page_manager().header().free_page_count, 0);

    // Reused pages match original set (in reverse order)
    pages.reverse();
    assert_eq!(reused_pages, pages);
}

#[test]
fn test_btree_leaf_merge_reclaims_page() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_btree_leaf_reclaim.treedb");
    let mut pool = create_test_pool(10, &db_path);
    let mut tree = BPlusTree::new_empty();

    // Insert 255 keys to force 1 leaf split -> creates root internal + 2 leaves (pages 2, 3, 4)
    for i in 1..=255 {
        tree.insert(&mut pool, i, RecordId::new(i as u32, 0))
            .unwrap();
    }
    tree.validate_tree(&mut pool).expect("tree valid");
    let peak_page_count = pool.page_count();

    // Delete keys until underflow causes leaf merge
    for i in 200..=255 {
        tree.delete(&mut pool, i).unwrap();
    }
    tree.validate_tree(&mut pool).expect("tree valid");

    // Leaf merge reclaimed at least 1 page into freelist!
    assert!(pool.page_manager().header().free_page_count > 0);
    pool.validate_freelist().expect("freelist valid");

    // Insert new keys to verify tree reuses freed page without growing file
    for i in 300..=400 {
        tree.insert(&mut pool, i, RecordId::new(i as u32, 0))
            .unwrap();
    }
    tree.validate_tree(&mut pool).expect("tree valid");
    assert_eq!(pool.page_count(), peak_page_count);
}

#[test]
fn test_btree_root_collapse_reclaims_page() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_root_collapse.treedb");
    let mut pool = create_test_pool(10, &db_path);
    let mut tree = BPlusTree::new_empty();

    // Single key -> root is leaf
    tree.insert(&mut pool, 10, RecordId::new(10, 0)).unwrap();
    assert_eq!(pool.page_count(), 3); // 0, 1, 2 (leaf root)

    // Delete final key -> tree becomes empty, leaf root is freed
    tree.delete(&mut pool, 10).unwrap();
    assert!(tree.is_empty());
    assert_eq!(pool.page_manager().header().free_page_count, 1);
    assert_eq!(pool.page_manager().header().freelist_head, 2);
    pool.validate_freelist().expect("freelist valid");

    // Re-inserting a key reuses page 2
    tree.insert(&mut pool, 20, RecordId::new(20, 0)).unwrap();
    assert_eq!(tree.root_page_id(), 2);
    assert_eq!(pool.page_manager().header().free_page_count, 0);
    assert_eq!(pool.page_count(), 3); // Still 3 pages!
}

#[test]
fn test_freelist_validation_matrix() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_validation.treedb");
    let mut pm = PageManager::create(&db_path).unwrap();

    let p2 = pm.allocate_page().unwrap();
    let p3 = pm.allocate_page().unwrap();
    pm.write_page(&Page::new(p2, PageType::Record)).unwrap();
    pm.write_page(&Page::new(p3, PageType::Record)).unwrap();

    FreePageManager::free_page(&mut pm, p2).unwrap();
    FreePageManager::free_page(&mut pm, p3).unwrap();
    FreePageManager::validate(&mut pm).expect("valid freelist passes");

    // 1. Corrupt count
    pm.header_mut().free_page_count = 99;
    pm.write_header_to_disk().unwrap();
    assert!(matches!(
        FreePageManager::validate(&mut pm),
        Err(Error::FreelistCorruption(_))
    ));
    pm.header_mut().free_page_count = 2;
    pm.write_header_to_disk().unwrap();

    // 2. Out-of-bounds head
    pm.header_mut().freelist_head = 999;
    pm.write_header_to_disk().unwrap();
    assert!(matches!(
        FreePageManager::validate(&mut pm),
        Err(Error::FreelistCorruption(_))
    ));
    pm.header_mut().freelist_head = 3;
    pm.write_header_to_disk().unwrap();

    // 3. Reserved page in freelist
    pm.header_mut().freelist_head = 1;
    pm.write_header_to_disk().unwrap();
    assert!(matches!(
        FreePageManager::validate(&mut pm),
        Err(Error::FreelistCorruption(_))
    ));
}

#[test]
fn test_freelist_pin_discipline() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_pins.treedb");
    let mut pool = create_test_pool(5, &db_path);

    let p2 = pool.allocate_page(PageType::Record).unwrap();
    assert_eq!(pool.pin_count(p2).unwrap(), 1);

    // Attempting to free a pinned page returns error
    let err = pool.free_page(p2).unwrap_err();
    assert!(matches!(err, Error::PageCorruption { .. }));

    pool.unpin_page(p2, false).unwrap();
    assert_eq!(pool.pin_count(p2).unwrap(), 0);

    // Now freeing succeeds
    pool.free_page(p2).unwrap();
    assert!(!pool.is_resident(p2));
}

#[test]
fn test_btree_internal_merge_reclaims_page() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_internal_merge.treedb");
    let mut pool = create_test_pool(20, &db_path);
    let mut tree = BPlusTree::new_empty();

    // Insert enough keys to generate a 3-level tree with multiple internal nodes
    // 254 entries per leaf, so 1500 keys will produce ~6-7 leaves and multiple internal nodes
    for i in 1..=1500 {
        tree.insert(&mut pool, i, RecordId::new(i as u32, 0))
            .unwrap();
    }
    tree.validate_tree(&mut pool).expect("tree valid");
    let peak_pages = pool.page_count();

    // Delete keys in batches to cause internal nodes to underflow and merge
    for i in (1..=1500).step_by(2) {
        tree.delete(&mut pool, i).unwrap();
    }
    tree.validate_tree(&mut pool)
        .expect("tree valid after deletions");

    // Free pages should have been reclaimed
    assert!(pool.page_manager().header().free_page_count > 0);
    pool.validate_freelist().expect("freelist valid");

    // Insert new keys to verify tree reuses pages safely
    for i in 2000..=2500 {
        tree.insert(&mut pool, i, RecordId::new(i as u32, 0))
            .unwrap();
    }
    tree.validate_tree(&mut pool)
        .expect("tree valid after reinsertion");
    assert!(pool.page_count() <= peak_pages + 2);
}

#[test]
fn test_freelist_wal_crash_recovery() {
    use treedb::engine::TreeDb;

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_freelist_wal.treedb");

    // 1. Create TreeDb, insert keys, commit, and delete to trigger reclamation
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        for i in 1..=300 {
            db.put(i, b"hello world").unwrap();
        }
        db.sync().unwrap();

        // Delete keys to trigger page reclamation
        for i in 150..=300 {
            db.delete(i).unwrap();
        }
        db.sync().unwrap();
    }

    // 2. Reopen and verify crash recovery preserves valid freelist and tree
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        for i in 1..150 {
            assert_eq!(db.get(i).unwrap(), Some(b"hello world".to_vec()));
        }
        for i in 150..=300 {
            assert_eq!(db.get(i).unwrap(), None);
        }
        db.buffer_pool_mut()
            .validate_freelist()
            .expect("freelist valid after recovery");
    }
}
