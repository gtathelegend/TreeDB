use std::collections::HashMap;
use tempfile::tempdir;

use treedb::{
    BufferPool, Error, Key, OverflowDescriptor, OverflowPage, Page, PageManager, PageType,
    RecordPage, RecordStore, TreeDb, MAX_RECORD_SIZE, NO_OVERFLOW_PAGE, OVERFLOW_DESCRIPTOR_SIZE,
    OVERFLOW_HEADER_SIZE, OVERFLOW_PAYLOAD_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE,
};

fn create_test_db(name: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = dir.path().join(format!("{name}.treedb.wal"));
    (dir, db_path, wal_path)
}

fn create_test_pool(capacity: usize) -> (BufferPool, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("test_pool.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");
    let pool = BufferPool::new(capacity, pm).expect("create buffer pool");
    (pool, dir, db_path)
}

#[test]
fn test_overflow_constants_and_page_layout() {
    assert_eq!(PAGE_SIZE, 4096);
    assert_eq!(PAGE_PAYLOAD_SIZE, 4080);
    assert_eq!(OVERFLOW_HEADER_SIZE, 8);
    assert_eq!(OVERFLOW_PAYLOAD_SIZE, 4072);
    assert_eq!(MAX_RECORD_SIZE, 4070);
    assert_eq!(OVERFLOW_DESCRIPTOR_SIZE, 8);
    assert_eq!(NO_OVERFLOW_PAGE, 0);

    // Verify mathematical derivations
    assert_eq!(
        OVERFLOW_PAYLOAD_SIZE,
        PAGE_PAYLOAD_SIZE - OVERFLOW_HEADER_SIZE
    );
    assert_eq!(
        MAX_RECORD_SIZE,
        PAGE_PAYLOAD_SIZE - treedb::RECORD_PAGE_HEADER_SIZE - treedb::SLOT_SIZE
    );
}

#[test]
fn test_overflow_page_serialization_and_validation() {
    let mut page = Page::new(10, PageType::Overflow);
    let raw_data = b"Arbitrary payload bytes for overflow page serialization test";
    let op = OverflowPage::new(25, raw_data).expect("construct");

    assert_eq!(op.next_page_id(), 25);
    assert_eq!(op.bytes_used(), raw_data.len());
    assert_eq!(op.payload(), raw_data);

    op.serialize(&mut page).expect("serialize");
    assert_eq!(page.page_type(), PageType::Overflow);

    let restored = OverflowPage::deserialize(&page).expect("deserialize");
    assert_eq!(restored, op);
    assert_eq!(restored.next_page_id(), 25);
    assert_eq!(restored.bytes_used(), raw_data.len());
    assert_eq!(restored.payload(), raw_data);

    // Corrupt page type rejection
    page.set_page_type(PageType::Record);
    let err = OverflowPage::deserialize(&page).unwrap_err();
    assert!(matches!(err, Error::PageCorruption { .. }));
}

#[test]
fn test_overflow_boundary_sizes() {
    let (_dir, db_path, _wal_path) = create_test_db("boundary_sizes");
    let mut db = TreeDb::create(&db_path, 20).expect("create db");

    let boundary_sizes = vec![
        0,                             // empty value
        1,                             // 1 byte
        MAX_RECORD_SIZE - 1,           // 4069 bytes (inline)
        MAX_RECORD_SIZE,               // 4070 bytes (inline max)
        MAX_RECORD_SIZE + 1,           // 4071 bytes (overflow 1 page)
        OVERFLOW_PAYLOAD_SIZE,         // 4072 bytes (overflow 1 full page)
        OVERFLOW_PAYLOAD_SIZE + 1,     // 4073 bytes (overflow 2 pages)
        2 * OVERFLOW_PAYLOAD_SIZE,     // 8144 bytes (overflow 2 full pages)
        2 * OVERFLOW_PAYLOAD_SIZE + 1, // 8145 bytes (overflow 3 pages)
        10 * OVERFLOW_PAYLOAD_SIZE,    // 40720 bytes (overflow 10 pages)
    ];

    for (idx, &size) in boundary_sizes.iter().enumerate() {
        let key = (idx + 1) as Key;
        let mut value = vec![0u8; size];
        for (i, b) in value.iter_mut().enumerate() {
            *b = ((i + idx) % 251) as u8;
        }

        db.put(key, &value).expect("put boundary value");
        let retrieved = db.get(key).expect("get boundary value");
        assert_eq!(retrieved, Some(value), "mismatch for boundary size {size}");
    }
}

#[test]
fn test_overflow_single_and_multi_page_chain() {
    let (mut pool, _dir, _db_path) = create_test_pool(20);
    let mut store = RecordStore::new();

    // 1. Single overflow page (4071 bytes)
    let val_4071 = vec![0x33u8; 4071];
    let rid_1 = store.insert(&mut pool, &val_4071).expect("insert 4071B");

    let desc_1 = {
        let page = pool.fetch_page(rid_1.page_id).expect("fetch page");
        let rp = RecordPage::deserialize(page).expect("deser");
        let desc = rp.get_overflow_descriptor(rid_1.slot_id).expect("desc");
        pool.unpin_page(rid_1.page_id, false).expect("unpin");
        desc
    };
    assert_eq!(desc_1.total_length, 4071);

    let chain_1 = store
        .validate_overflow_chain(&mut pool, desc_1.head_page_id, desc_1.total_length)
        .expect("validate chain 1");
    assert_eq!(chain_1.len(), 1);

    let retrieved_1 = store.get(&mut pool, rid_1).expect("get 1");
    assert_eq!(retrieved_1, Some(val_4071));

    // 2. Multi overflow pages (12,000 bytes -> 3 overflow pages)
    let val_12k = vec![0x77u8; 12000];
    let rid_2 = store.insert(&mut pool, &val_12k).expect("insert 12KB");

    let desc_2 = {
        let page = pool.fetch_page(rid_2.page_id).expect("fetch page");
        let rp = RecordPage::deserialize(page).expect("deser");
        let desc = rp.get_overflow_descriptor(rid_2.slot_id).expect("desc");
        pool.unpin_page(rid_2.page_id, false).expect("unpin");
        desc
    };
    assert_eq!(desc_2.total_length, 12000);

    let chain_2 = store
        .validate_overflow_chain(&mut pool, desc_2.head_page_id, desc_2.total_length)
        .expect("validate chain 2");
    assert_eq!(chain_2.len(), 3); // 4072 + 4072 + 3856 = 12000

    let retrieved_2 = store.get(&mut pool, rid_2).expect("get 2");
    assert_eq!(retrieved_2, Some(val_12k));
}

#[test]
fn test_overflow_persistence_across_close_reopen() {
    let (_dir, db_path, _wal_path) = create_test_db("persistence");

    let mut entries = HashMap::new();
    entries.insert(10, vec![0x11u8; 100]); // inline
    entries.insert(20, vec![0x22u8; MAX_RECORD_SIZE]); // inline max
    entries.insert(30, vec![0x33u8; 5000]); // overflow 2 pages
    entries.insert(40, vec![0x44u8; 15000]); // overflow 4 pages
    entries.insert(50, vec![0x55u8; 30000]); // overflow 8 pages

    {
        let mut db = TreeDb::create(&db_path, 20).expect("create db");
        for (&k, v) in &entries {
            db.put(k, v).expect("put");
        }
        db.sync().expect("sync");
    }

    {
        let mut db = TreeDb::open(&db_path, 20).expect("open db");
        for (&k, expected) in &entries {
            let actual = db.get(k).expect("get");
            assert_eq!(
                actual.as_ref(),
                Some(expected),
                "key {k} mismatch on reopen"
            );
        }
    }
}

#[test]
fn test_overflow_update_transitions() {
    let (_dir, db_path, _wal_path) = create_test_db("update_transitions");
    let mut db = TreeDb::create(&db_path, 20).expect("create db");

    let key = 100 as Key;
    let small_v1 = vec![0xAAu8; 50];
    let small_v2 = vec![0xBBu8; 200];
    let large_v1 = vec![0xCCu8; 10000];
    let large_v2 = vec![0xDDu8; 18000];

    // 1. Initial Small
    db.put(key, &small_v1).expect("put small_v1");
    assert_eq!(db.get(key).unwrap(), Some(small_v1));

    // 2. Small -> Small
    db.put(key, &small_v2).expect("update small_v2");
    assert_eq!(db.get(key).unwrap(), Some(small_v2));

    // 3. Small -> Large
    db.put(key, &large_v1).expect("update large_v1");
    assert_eq!(db.get(key).unwrap(), Some(large_v1));

    // 4. Large -> Large
    db.put(key, &large_v2).expect("update large_v2");
    assert_eq!(db.get(key).unwrap(), Some(large_v2));

    // 5. Large -> Small
    let small_v3 = vec![0xEEu8; 120];
    db.put(key, &small_v3).expect("update small_v3");
    assert_eq!(db.get(key).unwrap(), Some(small_v3));
}

#[test]
fn test_overflow_delete_and_freelist_recycling() {
    let (mut pool, _dir, _db_path) = create_test_pool(30);
    let mut store = RecordStore::new();

    // Insert 20,000-byte record (needs 5 overflow pages: 4072*4 = 16288 + 3712)
    let large_val = vec![0x99u8; 20000];
    let rid = store.insert(&mut pool, &large_val).expect("insert large");

    let initial_free_count = pool.page_manager().header().free_page_count;
    assert_eq!(initial_free_count, 0);

    let page_count_before = pool.page_count();

    // Delete large record
    let deleted = store.delete(&mut pool, rid).expect("delete");
    assert!(deleted);

    // Overflow pages (5 pages) must have entered the persistent freelist!
    let free_count_after_delete = pool.page_manager().header().free_page_count;
    assert_eq!(free_count_after_delete, 5);

    pool.validate_freelist().expect("freelist valid");

    // Insert another 20,000-byte record
    let large_val_2 = vec![0x88u8; 20000];
    let rid_2 = store
        .insert(&mut pool, &large_val_2)
        .expect("insert large 2");

    // All 5 free pages should have been recycled from the freelist!
    let free_count_after_reuse = pool.page_manager().header().free_page_count;
    assert_eq!(free_count_after_reuse, 0);

    // Database total page count must not have grown for the overflow pages
    let page_count_after = pool.page_count();
    assert_eq!(page_count_before, page_count_after);

    let read_back = store.get(&mut pool, rid_2).expect("get");
    assert_eq!(read_back, Some(large_val_2));
}

#[test]
fn test_overflow_reused_page_reinitialization() {
    let (mut pool, _dir, _db_path) = create_test_pool(20);
    let mut store = RecordStore::new();

    // 1. Create and delete an overflow record
    let large_val = vec![0xFFu8; 10000]; // 3 overflow pages
    let rid = store.insert(&mut pool, &large_val).expect("insert");
    store.delete(&mut pool, rid).expect("delete");

    assert_eq!(pool.page_manager().header().free_page_count, 3);

    // 2. Reallocate recycled pages as Record, Leaf, Internal
    let p_record = pool.allocate_page(PageType::Record).expect("alloc record");
    let p_leaf = pool.allocate_page(PageType::Leaf).expect("alloc leaf");
    let p_internal = pool
        .allocate_page(PageType::Internal)
        .expect("alloc internal");

    // Verify each reused page is zeroed and stamped with the new page type
    {
        let page_r = pool.fetch_page(p_record).expect("fetch");
        assert_eq!(page_r.page_type(), PageType::Record);
        assert_eq!(page_r.payload(), &[0u8; PAGE_PAYLOAD_SIZE][..]);
        pool.unpin_page(p_record, false).expect("unpin");
    }
    {
        let page_l = pool.fetch_page(p_leaf).expect("fetch");
        assert_eq!(page_l.page_type(), PageType::Leaf);
        assert_eq!(page_l.payload(), &[0u8; PAGE_PAYLOAD_SIZE][..]);
        pool.unpin_page(p_leaf, false).expect("unpin");
    }
    {
        let page_i = pool.fetch_page(p_internal).expect("fetch");
        assert_eq!(page_i.page_type(), PageType::Internal);
        assert_eq!(page_i.payload(), &[0u8; PAGE_PAYLOAD_SIZE][..]);
        pool.unpin_page(p_internal, false).expect("unpin");
    }
}

#[test]
fn test_overflow_record_page_compaction_stability() {
    let (mut pool, _dir, _db_path) = create_test_pool(20);
    let target_page_id = pool.allocate_page(PageType::Record).expect("alloc");
    pool.unpin_page(target_page_id, true).expect("unpin");

    let mut rp = RecordPage::new();
    let s0 = rp.insert(b"INLINE_0").expect("s0");
    let s1 = rp.insert_overflow_descriptor(50, 15000).expect("s1");
    let s2 = rp.insert(b"INLINE_2").expect("s2");
    let s3 = rp.insert_overflow_descriptor(60, 25000).expect("s3");

    // Delete s0 and s2
    assert!(rp.delete(s0));
    assert!(rp.delete(s2));

    // Compact record page
    rp.compact().expect("compact");

    // Stable slot references and descriptors preserved
    assert_eq!(rp.get(s0), None);
    assert_eq!(rp.get(s2), None);
    assert_eq!(
        rp.get_overflow_descriptor(s1),
        Some(OverflowDescriptor {
            head_page_id: 50,
            total_length: 15000,
        })
    );
    assert_eq!(
        rp.get_overflow_descriptor(s3),
        Some(OverflowDescriptor {
            head_page_id: 60,
            total_length: 25000,
        })
    );
}

#[test]
fn test_overflow_scan_transparent() {
    let (_dir, db_path, _wal_path) = create_test_db("scan_transparent");
    let mut db = TreeDb::create(&db_path, 30).expect("create db");

    let test_data = vec![
        (10, vec![0x10u8; 10]),
        (20, vec![0x20u8; 5000]),  // overflow
        (30, vec![0x30u8; 4070]),  // inline max
        (40, vec![0x40u8; 12000]), // overflow
        (50, vec![0x50u8; 0]),     // empty
        (60, vec![0x60u8; 20000]), // overflow
    ];

    for (k, v) in &test_data {
        db.put(*k, v).expect("put");
    }

    let scanned = db.scan(Some(15), Some(55)).expect("scan");
    assert_eq!(scanned.len(), 4);
    assert_eq!(scanned[0], (20, vec![0x20u8; 5000]));
    assert_eq!(scanned[1], (30, vec![0x30u8; 4070]));
    assert_eq!(scanned[2], (40, vec![0x40u8; 12000]));
    assert_eq!(scanned[3], (50, vec![0x50u8; 0]));
}

#[test]
fn test_overflow_cycle_detection_and_corruption_handling() {
    let (mut pool, _dir, _db_path) = create_test_pool(20);
    let store = RecordStore::new();

    // 1. Test cycle detection A -> B -> A
    let p_a = pool.allocate_page(PageType::Overflow).expect("alloc A");
    let p_b = pool.allocate_page(PageType::Overflow).expect("alloc B");

    let op_a = OverflowPage::new(p_b, &vec![0x11; OVERFLOW_PAYLOAD_SIZE]).expect("op_a");
    let op_b = OverflowPage::new(p_a, &vec![0x22; OVERFLOW_PAYLOAD_SIZE]).expect("op_b"); // points back to A!

    {
        let page_a = pool.fetch_page_mut(p_a).unwrap();
        op_a.serialize(page_a).unwrap();
        pool.unpin_page(p_a, true).unwrap();
    }
    {
        let page_b = pool.fetch_page_mut(p_b).unwrap();
        op_b.serialize(page_b).unwrap();
        pool.unpin_page(p_b, true).unwrap();
    }

    let cycle_err = store
        .validate_overflow_chain(&mut pool, p_a, 10000)
        .unwrap_err();
    assert!(matches!(cycle_err, Error::OverflowCorruption(msg) if msg.contains("cycle detected")));

    // 2. Test out-of-bounds pointer
    let p_c = pool.allocate_page(PageType::Overflow).expect("alloc C");
    let op_c = OverflowPage::new(9999, &vec![0x33; OVERFLOW_PAYLOAD_SIZE]).expect("op_c");
    {
        let page_c = pool.fetch_page_mut(p_c).unwrap();
        op_c.serialize(page_c).unwrap();
        pool.unpin_page(p_c, true).unwrap();
    }
    let oob_err = store
        .validate_overflow_chain(&mut pool, p_c, 5000)
        .unwrap_err();
    assert!(matches!(oob_err, Error::OverflowCorruption(msg) if msg.contains("out-of-bounds")));

    // 3. Test wrong page type in chain
    let p_d = pool.allocate_page(PageType::Record).expect("alloc D");
    let op_head = OverflowPage::new(p_d, &vec![0x44; OVERFLOW_PAYLOAD_SIZE]).expect("op_head");
    let p_head = pool.allocate_page(PageType::Overflow).expect("alloc head");
    {
        let page_h = pool.fetch_page_mut(p_head).unwrap();
        op_head.serialize(page_h).unwrap();
        pool.unpin_page(p_head, true).unwrap();
        pool.unpin_page(p_d, false).unwrap();
    }
    let type_err = store
        .validate_overflow_chain(&mut pool, p_head, 5000)
        .unwrap_err();
    assert!(matches!(type_err, Error::PageCorruption { .. }));
}

#[test]
fn test_overflow_crash_recovery_uncommitted_insert() {
    let (_dir, db_path, _wal_path) = create_test_db("crash_uncommitted_insert");

    let initial_val = vec![0x11u8; 100];
    let large_val = vec![0x55u8; 16000];

    // 1. Initial committed database
    {
        let mut db = TreeDb::create(&db_path, 20).expect("create db");
        db.put(1, &initial_val).expect("put initial");
        db.sync().expect("sync initial");
    }

    // 2. Uncommitted insert of large record (crashes without commit)
    {
        let mut db = TreeDb::open(&db_path, 20).expect("open db");
        let _txn = db.begin_txn().expect("begin");
        db.put(42, &large_val).expect("put large");
        db.buffer_pool_mut().flush_all().expect("flush");
        // Drop without commit
    }

    // 3. Reopen and verify key 42 was rolled back and key 1 exists
    {
        let mut db = TreeDb::open(&db_path, 20).expect("reopen and recover");
        assert_eq!(db.get(42).expect("get 42"), None);
        assert_eq!(db.get(1).expect("get 1"), Some(initial_val));
        db.sync().expect("sync");
    }
}

#[test]
fn test_overflow_crash_recovery_uncommitted_update() {
    let (_dir, db_path, _wal_path) = create_test_db("crash_uncommitted_update");

    let initial_large = vec![0x55u8; 16000];
    let updated_large = vec![0x77u8; 25000];

    // 1. Initial committed large record
    {
        let mut db = TreeDb::create(&db_path, 20).expect("create db");
        db.put(42, &initial_large).expect("put large");
        db.sync().expect("sync initial");
    }

    // 2. Uncommitted update (crashes without commit)
    {
        let mut db = TreeDb::open(&db_path, 20).expect("open db");
        let _txn = db.begin_txn().expect("begin");
        db.put(42, &updated_large).expect("put updated large");
        db.buffer_pool_mut().flush_all().expect("flush");
        // Drop without commit
    }

    // 3. Reopen and verify key 42 rolled back to initial_large
    {
        let mut db = TreeDb::open(&db_path, 20).expect("reopen and recover");
        assert_eq!(db.get(42).expect("get 42"), Some(initial_large));
        db.sync().expect("sync");
    }
}

#[test]
fn test_overflow_crash_recovery_uncommitted_delete() {
    let (_dir, db_path, _wal_path) = create_test_db("crash_uncommitted_delete");

    let initial_val = vec![0x11u8; 100];
    let large_val = vec![0x55u8; 16000];

    // 1. Initial committed large record and anchor key
    {
        let mut db = TreeDb::create(&db_path, 20).expect("create db");
        db.put(1, &initial_val).expect("put anchor");
        db.put(42, &large_val).expect("put large");
        db.sync().expect("sync initial");
    }

    // 2. Uncommitted delete (crashes without commit)
    {
        let mut db = TreeDb::open(&db_path, 20).expect("open db");
        let _txn = db.begin_txn().expect("begin");
        let deleted = db.delete(42).expect("delete");
        assert_eq!(deleted, Some(large_val.clone()));
        db.buffer_pool_mut().flush_all().expect("flush");
        // Drop without commit
    }

    // 3. Reopen and verify key 42 was restored!
    {
        let mut db = TreeDb::open(&db_path, 20).expect("reopen and recover");
        assert_eq!(db.get(42).expect("get 42"), Some(large_val));
        db.sync().expect("sync");
    }
}

#[test]
fn test_overflow_crash_recovery_committed_delete() {
    let (_dir, db_path, _wal_path) = create_test_db("crash_committed_delete");

    let large_val = vec![0x55u8; 16000];

    // 1. Initial committed large record
    {
        let mut db = TreeDb::create(&db_path, 20).expect("create db");
        db.put(42, &large_val).expect("put large");
        db.sync().expect("sync initial");
    }

    // 2. Committed delete
    {
        let mut db = TreeDb::open(&db_path, 20).expect("open db");
        let txn = db.begin_txn().expect("begin");
        let deleted = db.delete(42).expect("delete");
        assert_eq!(deleted, Some(large_val));
        db.commit_txn(txn).expect("commit");
        db.sync().expect("sync");
    }

    // 3. Reopen and verify key 42 is absent
    {
        let mut db = TreeDb::open(&db_path, 20).expect("reopen");
        assert_eq!(db.get(42).expect("get 42"), None);
        db.sync().expect("sync");
    }
}

#[test]
fn test_overflow_pin_discipline() {
    let (mut pool, _dir, _db_path) = create_test_pool(10);
    let mut store = RecordStore::new();

    let val = vec![0x12u8; 15000];
    let rid = store.insert(&mut pool, &val).expect("insert");

    // All pages unpinned
    for pid in 0..pool.page_count() {
        if pool.is_resident(pid) {
            assert_eq!(
                pool.pin_count(pid).unwrap(),
                0,
                "page {pid} left pinned after insert"
            );
        }
    }

    let _ = store.get(&mut pool, rid).expect("get");
    for pid in 0..pool.page_count() {
        if pool.is_resident(pid) {
            assert_eq!(
                pool.pin_count(pid).unwrap(),
                0,
                "page {pid} left pinned after get"
            );
        }
    }

    let _ = store.delete(&mut pool, rid).expect("delete");
    for pid in 0..pool.page_count() {
        if pool.is_resident(pid) {
            assert_eq!(
                pool.pin_count(pid).unwrap(),
                0,
                "page {pid} left pinned after delete"
            );
        }
    }
}

#[test]
fn test_overflow_randomized_oracle_comparison() {
    let (_dir, db_path, _wal_path) = create_test_db("randomized_oracle");
    let mut db = TreeDb::create(&db_path, 30).expect("create db");
    let mut oracle: HashMap<Key, Vec<u8>> = HashMap::new();

    let mut rng_state: u64 = 0xDEAD_BEEF_CAFE_1234;
    let mut next_rand = || -> u64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    let sizes = [
        0,
        16,
        64,
        MAX_RECORD_SIZE - 1,
        MAX_RECORD_SIZE,
        MAX_RECORD_SIZE + 1,
        OVERFLOW_PAYLOAD_SIZE,
        OVERFLOW_PAYLOAD_SIZE + 500,
        2 * OVERFLOW_PAYLOAD_SIZE + 100,
        3 * OVERFLOW_PAYLOAD_SIZE,
    ];

    for op_idx in 0..200 {
        let op_type = next_rand() % 10;

        match op_type {
            0..=5 => {
                // Insert or Update
                let key = (next_rand() % 50 + 1) as Key;
                let size_choice = sizes[(next_rand() as usize) % sizes.len()];
                let mut val = vec![0u8; size_choice];
                for b in &mut val {
                    *b = (next_rand() % 256) as u8;
                }

                db.put(key, &val).expect("put");
                oracle.insert(key, val);
            }
            6..=7 => {
                // Delete
                let key = (next_rand() % 50 + 1) as Key;
                let expected = oracle.remove(&key);
                let actual = db.delete(key).expect("delete");
                assert_eq!(
                    actual, expected,
                    "delete mismatch at op {op_idx} for key {key}"
                );
            }
            8..=9 => {
                // Get
                let key = (next_rand() % 50 + 1) as Key;
                let expected = oracle.get(&key).cloned();
                let actual = db.get(key).expect("get");
                assert_eq!(
                    actual, expected,
                    "get mismatch at op {op_idx} for key {key}"
                );
            }
            _ => unreachable!(),
        }

        // Periodically verify scan
        if op_idx % 50 == 0 {
            let mut expected_pairs: Vec<(Key, Vec<u8>)> =
                oracle.iter().map(|(&k, v)| (k, v.clone())).collect();
            expected_pairs.sort_by_key(|&(k, _)| k);

            let actual_pairs = db.scan(None, None).expect("scan");
            assert_eq!(actual_pairs, expected_pairs, "scan mismatch at op {op_idx}");
        }
    }

    // Sync, close, reopen, and verify entire oracle
    db.sync().expect("sync");
    drop(db);

    let mut reopened_db = TreeDb::open(&db_path, 30).expect("reopen");
    for (&key, expected_val) in &oracle {
        let actual_val = reopened_db.get(key).expect("get on reopen");
        assert_eq!(
            actual_val.as_ref(),
            Some(expected_val),
            "reopen mismatch for key {key}"
        );
    }
}
