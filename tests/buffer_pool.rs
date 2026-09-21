use tempfile::tempdir;
use treedb::{
    BufferPool, Error, PageManager, PageType, DEFAULT_METADATA_PAGE_ID, HEADER_PAGE_ID, MAGIC_BYTES,
};

fn create_test_pool(capacity: usize) -> (BufferPool, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("test_bp.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");
    let pool = BufferPool::new(capacity, pm).expect("create buffer pool");
    (pool, dir, db_path)
}

#[test]
fn test_buffer_pool_construction() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("test_construct.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");

    // Capacity > 0 succeeds
    let pool = BufferPool::new(10, pm).expect("should succeed");
    assert_eq!(pool.capacity(), 10);
    assert_eq!(pool.stats().capacity, 10);
    assert_eq!(pool.stats().hits, 0);
    assert_eq!(pool.stats().misses, 0);

    // Capacity == 0 fails
    let pm2 = PageManager::open(&db_path).expect("open pm");
    let err = BufferPool::new(0, pm2).unwrap_err();
    assert!(matches!(err, Error::InvalidCapacity(0)));
}

#[test]
fn test_fetch_hit_and_miss() {
    let (mut pool, _dir, _path) = create_test_pool(5);

    // Page 0 (Header) miss on first fetch
    let p0 = pool.fetch_page(HEADER_PAGE_ID).expect("fetch page 0");
    assert_eq!(&p0.as_bytes()[0..8], MAGIC_BYTES);
    assert!(pool.is_resident(HEADER_PAGE_ID));
    assert_eq!(pool.stats().misses, 1);
    assert_eq!(pool.stats().hits, 0);

    // Second fetch of Page 0 is a hit
    let p0_again = pool.fetch_page(HEADER_PAGE_ID).expect("fetch page 0 again");
    assert_eq!(&p0_again.as_bytes()[0..8], MAGIC_BYTES);
    assert_eq!(pool.stats().hits, 1);
    assert_eq!(pool.stats().misses, 1);
    assert_eq!(pool.stats().hit_rate(), 0.5);

    // Unpin both fetches
    pool.unpin_page(HEADER_PAGE_ID, false).expect("unpin 1");
    pool.unpin_page(HEADER_PAGE_ID, false).expect("unpin 2");
    assert_eq!(pool.pin_count(HEADER_PAGE_ID).unwrap(), 0);
}

#[test]
fn test_pinning_lifecycle_and_underflow() {
    let (mut pool, _dir, _path) = create_test_pool(3);

    // Fetch Page 1 (pin_count = 1)
    pool.fetch_page(DEFAULT_METADATA_PAGE_ID).expect("fetch 1");
    assert_eq!(pool.pin_count(DEFAULT_METADATA_PAGE_ID).unwrap(), 1);
    assert!(pool.is_pinned(DEFAULT_METADATA_PAGE_ID).unwrap());

    // Fetch again (pin_count = 2)
    pool.fetch_page(DEFAULT_METADATA_PAGE_ID).expect("fetch 2");
    assert_eq!(pool.pin_count(DEFAULT_METADATA_PAGE_ID).unwrap(), 2);

    // Unpin once (pin_count = 1)
    pool.unpin_page(DEFAULT_METADATA_PAGE_ID, false)
        .expect("unpin 1");
    assert_eq!(pool.pin_count(DEFAULT_METADATA_PAGE_ID).unwrap(), 1);

    // Unpin second time (pin_count = 0)
    pool.unpin_page(DEFAULT_METADATA_PAGE_ID, false)
        .expect("unpin 2");
    assert_eq!(pool.pin_count(DEFAULT_METADATA_PAGE_ID).unwrap(), 0);
    assert!(!pool.is_pinned(DEFAULT_METADATA_PAGE_ID).unwrap());

    // Unpin third time -> PinCountUnderflow error
    let err = pool
        .unpin_page(DEFAULT_METADATA_PAGE_ID, false)
        .unwrap_err();
    assert!(matches!(err, Error::PinCountUnderflow(1)));
}

#[test]
fn test_dirty_page_tracking_and_flush() {
    let (mut pool, _dir, _path) = create_test_pool(3);

    // Allocate page 2 (installs as dirty, pin_count = 1)
    let p2_id = pool.allocate_page(PageType::Leaf).expect("alloc p2");
    assert_eq!(p2_id, 2);
    assert!(pool.is_dirty(p2_id).unwrap());
    assert_eq!(pool.stats().current_dirty_pages, 1);

    // Unpin with dirty = false should NOT clear an already dirty page
    pool.unpin_page(p2_id, false).expect("unpin");
    assert!(pool.is_dirty(p2_id).unwrap());

    // Explicit flush clears dirty state
    pool.flush_page(p2_id).expect("flush p2");
    assert!(!pool.is_dirty(p2_id).unwrap());
    assert_eq!(pool.stats().current_dirty_pages, 0);
    assert_eq!(pool.stats().dirty_flushes, 1);

    // Fetch mut marks dirty again
    {
        let p2_mut = pool.fetch_page_mut(p2_id).expect("fetch mut");
        p2_mut.payload_mut()[0..5].copy_from_slice(b"world");
    }
    assert!(pool.is_dirty(p2_id).unwrap());
    pool.unpin_page(p2_id, false).expect("unpin");
    assert!(pool.is_dirty(p2_id).unwrap());
}

#[test]
fn test_clean_eviction() {
    let (mut pool, _dir, _path) = create_test_pool(2);

    // Prepare pages 2, 3 on disk
    let p2_id = pool.allocate_page(PageType::Leaf).expect("alloc 2");
    let p3_id = pool.allocate_page(PageType::Leaf).expect("alloc 3");
    pool.flush_all().expect("flush all");
    pool.unpin_page(p2_id, false).expect("unpin 2");
    pool.unpin_page(p3_id, false).expect("unpin 3");

    // Clear pool resident set by creating new pool over same PM
    let pm = pool.page_manager().path().to_path_buf();
    drop(pool);
    let mut pool = BufferPool::new(2, PageManager::open(pm).unwrap()).unwrap();

    // Load page 0 and page 1 (capacity = 2)
    pool.fetch_page(0).expect("fetch 0");
    pool.unpin_page(0, false).expect("unpin 0");

    pool.fetch_page(1).expect("fetch 1");
    pool.unpin_page(1, false).expect("unpin 1");

    assert!(pool.is_resident(0));
    assert!(pool.is_resident(1));

    // Re-access page 0 to make it more recently used than page 1
    pool.fetch_page(0).expect("fetch 0");
    pool.unpin_page(0, false).expect("unpin 0");

    // Fetch page 2 -> should evict page 1 (the least recently used)
    pool.fetch_page(2).expect("fetch 2");
    pool.unpin_page(2, false).expect("unpin 2");

    assert!(pool.is_resident(0), "page 0 should remain resident");
    assert!(pool.is_resident(2), "page 2 should be resident");
    assert!(!pool.is_resident(1), "page 1 should have been evicted");
    assert_eq!(pool.stats().evictions, 1);
}

#[test]
fn test_dirty_eviction_flushes_to_disk() {
    let (mut pool, _dir, db_path) = create_test_pool(2);

    // Allocate page 2 and page 3
    let p2_id = pool.allocate_page(PageType::Leaf).expect("alloc 2");
    let p3_id = pool.allocate_page(PageType::Leaf).expect("alloc 3");

    // Write distinctive payload into page 2
    {
        let p2_mut = pool.fetch_page_mut(p2_id).expect("fetch p2 mut");
        p2_mut.payload_mut()[0..13].copy_from_slice(b"dirty evicted");
    }
    // Unpin page 2 (pin count was 2 because allocate_page pins once and fetch_page_mut pins once)
    pool.unpin_page(p2_id, false).expect("unpin 1");
    pool.unpin_page(p2_id, false).expect("unpin 2");
    assert_eq!(pool.pin_count(p2_id).unwrap(), 0);
    assert!(pool.is_dirty(p2_id).unwrap());

    // Unpin page 3
    pool.unpin_page(p3_id, false).expect("unpin p3");

    // Re-access page 3 to make page 2 the LRU victim
    pool.fetch_page(p3_id).expect("touch p3");
    pool.unpin_page(p3_id, false).expect("unpin p3");

    // Fetch page 0 -> causes eviction of page 2
    pool.fetch_page(0).expect("fetch 0");
    pool.unpin_page(0, false).expect("unpin 0");

    assert!(!pool.is_resident(p2_id), "page 2 should be evicted");
    assert_eq!(pool.stats().evictions, 1);
    assert!(pool.stats().dirty_flushes >= 1);

    // Now bypass buffer pool, open file directly with PageManager and verify data was written
    let mut direct_pm = PageManager::open(&db_path).expect("open pm");
    let disk_p2 = direct_pm.read_page(p2_id).expect("read disk p2");
    assert_eq!(&disk_p2.payload()[0..13], b"dirty evicted");
}

#[test]
fn test_pinned_protection_and_all_pinned_error() {
    let (mut pool, _dir, _path) = create_test_pool(2);

    // Fetch and pin page 0 and page 1
    pool.fetch_page(0).expect("pin 0");
    pool.fetch_page(1).expect("pin 1");

    // Allocate page 2 requires a free/evictable frame, but all 2 are pinned
    let err = pool.allocate_page(PageType::Leaf).unwrap_err();
    assert!(matches!(err, Error::NoEvictablePage));

    // Unpin page 1
    pool.unpin_page(1, false).expect("unpin 1");

    // Now allocation succeeds by evicting unpinned page 1 while page 0 stays pinned
    let p2_id = pool.allocate_page(PageType::Leaf).expect("alloc p2");
    assert_eq!(p2_id, 2);
    assert!(pool.is_resident(0));
    assert!(pool.is_pinned(0).unwrap());
    assert!(!pool.is_resident(1));
    assert!(pool.is_resident(2));
}

#[test]
fn test_deterministic_lru_replacement_sequence() {
    let (mut pool, _dir, _path) = create_test_pool(3);

    // Allocate pages 2, 3, 4
    let p2 = pool.allocate_page(PageType::Leaf).expect("alloc 2");
    let p3 = pool.allocate_page(PageType::Leaf).expect("alloc 3");
    let p4 = pool.allocate_page(PageType::Leaf).expect("alloc 4");
    pool.flush_all().expect("flush");

    pool.unpin_page(p2, false).expect("unpin 2");
    pool.unpin_page(p3, false).expect("unpin 3");
    pool.unpin_page(p4, false).expect("unpin 4");

    // Access sequence: touch p2, touch p3
    // Order of least-to-most recently used: [p4 (oldest), p2, p3 (newest)]
    pool.fetch_page(p2).expect("touch 2");
    pool.unpin_page(p2, false).expect("unpin 2");

    pool.fetch_page(p3).expect("touch 3");
    pool.unpin_page(p3, false).expect("unpin 3");

    // Load page 0 -> p4 must be evicted
    pool.fetch_page(0).expect("fetch 0");
    pool.unpin_page(0, false).expect("unpin 0");

    assert!(!pool.is_resident(p4));
    assert!(pool.is_resident(p2));
    assert!(pool.is_resident(p3));
    assert!(pool.is_resident(0));

    // Access p2 -> order: [p3, 0, p2]
    pool.fetch_page(p2).expect("touch 2");
    pool.unpin_page(p2, false).expect("unpin 2");

    // Load page 1 -> p3 must be evicted
    pool.fetch_page(1).expect("fetch 1");
    pool.unpin_page(1, false).expect("unpin 1");

    assert!(!pool.is_resident(p3));
    assert!(pool.is_resident(p2));
    assert!(pool.is_resident(0));
    assert!(pool.is_resident(1));
}

#[test]
fn test_flush_all_and_persistence() {
    let (mut pool, _dir, db_path) = create_test_pool(5);

    let p2 = pool.allocate_page(PageType::Leaf).expect("alloc 2");
    let p3 = pool.allocate_page(PageType::Leaf).expect("alloc 3");

    {
        let p2_mut = pool.fetch_page_mut(p2).expect("p2 mut");
        p2_mut.payload_mut()[0..4].copy_from_slice(b"PAGE");
    }
    pool.unpin_page(p2, false).expect("unpin 1");
    pool.unpin_page(p2, false).expect("unpin 2");

    {
        let p3_mut = pool.fetch_page_mut(p3).expect("p3 mut");
        p3_mut.payload_mut()[0..4].copy_from_slice(b"DATA");
    }
    pool.unpin_page(p3, false).expect("unpin 1");
    pool.unpin_page(p3, false).expect("unpin 2");

    assert_eq!(pool.stats().current_dirty_pages, 2);

    pool.flush_all().expect("flush all");
    assert_eq!(pool.stats().current_dirty_pages, 0);

    pool.sync().expect("sync");
    drop(pool);

    // Reopen with a fresh BufferPool
    let pm = PageManager::open(&db_path).expect("reopen pm");
    let mut new_pool = BufferPool::new(5, pm).expect("new pool");

    let p2_read = new_pool.fetch_page(p2).expect("fetch p2");
    assert_eq!(&p2_read.payload()[0..4], b"PAGE");

    let p3_read = new_pool.fetch_page(p3).expect("fetch p3");
    assert_eq!(&p3_read.payload()[0..4], b"DATA");
}

#[test]
fn test_capacity_one_boundary() {
    let (mut pool, _dir, _path) = create_test_pool(1);

    // Page 1
    let p1 = pool.fetch_page(1).expect("fetch 1");
    assert_eq!(p1.id(), 1);
    pool.unpin_page(1, false).expect("unpin 1");

    // Page 0 -> evicts Page 1 immediately
    let p0 = pool.fetch_page(0).expect("fetch 0");
    assert_eq!(&p0.as_bytes()[0..8], MAGIC_BYTES);
    assert!(!pool.is_resident(1));
    assert!(pool.is_resident(0));
    pool.unpin_page(0, false).expect("unpin 0");

    // Page 1 again -> evicts Page 0
    let p1_again = pool.fetch_page(1).expect("fetch 1 again");
    assert_eq!(p1_again.id(), 1);
    assert!(pool.is_resident(1));
    assert!(!pool.is_resident(0));
    assert_eq!(pool.stats().evictions, 2);
    pool.unpin_page(1, false).expect("unpin 1");
}

#[test]
fn test_high_volume_sequential_and_repeated_access() {
    let (mut pool, _dir, _path) = create_test_pool(4);

    // Allocate 30 pages
    let mut allocated_ids = Vec::new();
    for _ in 0..30 {
        let pid = pool.allocate_page(PageType::Record).expect("alloc");
        allocated_ids.push(pid);
        pool.unpin_page(pid, false).expect("unpin");
    }

    // Repeatedly touch pages in a loop
    for &pid in &allocated_ids {
        let page = pool.fetch_page(pid).expect("fetch");
        assert_eq!(page.id(), pid);
        assert_eq!(page.page_type(), PageType::Record);
        pool.unpin_page(pid, false).expect("unpin");
    }

    assert_eq!(pool.page_count(), 32); // 0 + 1 + 30
    assert!(pool.stats().evictions > 0);
}
