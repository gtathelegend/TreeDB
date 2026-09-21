use std::collections::HashMap;
use tempfile::tempdir;
use treedb::{
    BufferPool, DbHeader, Error, Page, PageId, PageManager, PageType, RecordId, RecordPage,
    RecordStore, TreeDb, MAX_RECORD_SIZE, RECORD_PAGE_HEADER_SIZE, SLOT_SIZE,
};

fn create_test_pool(capacity: usize) -> (BufferPool, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("test_record_store.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");
    let pool = BufferPool::new(capacity, pm).expect("create buffer pool");
    (pool, dir, db_path)
}

#[test]
fn test_record_constants() {
    assert_eq!(RECORD_PAGE_HEADER_SIZE, 6);
    assert_eq!(SLOT_SIZE, 4);
    assert_eq!(MAX_RECORD_SIZE, 4078);
    let header = DbHeader::default();
    assert_eq!(header.page_size, 4096);
}

#[test]
fn test_record_page_creation_and_free_space() {
    let rp = RecordPage::new();
    assert_eq!(rp.slot_count(), 0);
    assert_eq!(rp.active_record_count(), 0);
    assert_eq!(rp.contiguous_free_space(), 4082);
    assert_eq!(rp.total_free_space(), 4082);

    let mut page = Page::new(2, PageType::Record);
    rp.serialize(&mut page)
        .expect("serialize empty record page");

    let restored = RecordPage::deserialize(&page).expect("deserialize record page");
    assert_eq!(restored.slot_count(), 0);
    assert_eq!(restored.active_record_count(), 0);
    assert_eq!(restored.contiguous_free_space(), 4082);
    assert_eq!(restored.total_free_space(), 4082);
}

#[test]
fn test_record_page_max_and_oversized_record() {
    let mut rp = RecordPage::new();

    // Max record: 4078 bytes fits exactly
    let max_record = vec![0xAAu8; MAX_RECORD_SIZE];
    assert!(rp.can_fit(max_record.len()));
    let slot = rp.insert(&max_record).expect("insert max record");
    assert_eq!(slot, 0);
    assert_eq!(rp.get(slot), Some(&max_record[..]));
    assert_eq!(rp.contiguous_free_space(), 0);
    assert_eq!(rp.total_free_space(), 0);

    // Oversized record by 1 byte rejected with Error::ValueTooLarge
    let mut rp2 = RecordPage::new();
    let oversized = vec![0xBBu8; MAX_RECORD_SIZE + 1];
    assert!(!rp2.can_fit(oversized.len()));
    let err = rp2.insert(&oversized).unwrap_err();
    assert!(
        matches!(err, Error::ValueTooLarge { size, limit } if size == MAX_RECORD_SIZE + 1 && limit == MAX_RECORD_SIZE)
    );
}

#[test]
fn test_record_page_variable_lengths() {
    let mut rp = RecordPage::new();

    // 0 bytes (empty)
    let s0 = rp.insert(b"").expect("insert empty");
    assert_eq!(rp.get(s0), Some(&b""[..]));

    // 1 byte
    let s1 = rp.insert(b"X").expect("insert 1 byte");
    assert_eq!(rp.get(s1), Some(&b"X"[..]));

    // Small record (32 bytes)
    let small = [7u8; 32];
    let s2 = rp.insert(&small).expect("insert small");
    assert_eq!(rp.get(s2), Some(&small[..]));

    // Medium record (512 bytes)
    let medium = [42u8; 512];
    let s3 = rp.insert(&medium).expect("insert medium");
    assert_eq!(rp.get(s3), Some(&medium[..]));

    // Near capacity record (2000 bytes)
    let large = [99u8; 2000];
    let s4 = rp.insert(&large).expect("insert large");
    assert_eq!(rp.get(s4), Some(&large[..]));

    assert_eq!(rp.active_record_count(), 5);
}

#[test]
fn test_record_page_compaction_and_slot_stability() {
    let mut rp = RecordPage::new();

    let a = rp.insert(b"RECORD_A_12345").expect("insert A");
    let b = rp.insert(b"RECORD_B_67890").expect("insert B");
    let c = rp.insert(b"RECORD_C_ABCDE").expect("insert C");

    assert_eq!(a, 0);
    assert_eq!(b, 1);
    assert_eq!(c, 2);

    // Delete middle record B
    assert!(rp.delete(b));
    assert_eq!(rp.get(b), None);
    assert_eq!(rp.active_record_count(), 2);

    let free_before = rp.contiguous_free_space();
    rp.compact().expect("compact");
    let free_after = rp.contiguous_free_space();

    assert!(free_after > free_before);
    // Slot IDs and contents remain stable after compaction
    assert_eq!(rp.get(a), Some(&b"RECORD_A_12345"[..]));
    assert_eq!(rp.get(b), None);
    assert_eq!(rp.get(c), Some(&b"RECORD_C_ABCDE"[..]));
}

// ---------------------------------------------------------------------------
// 2. RecordStore Operations & BufferPool Discipline
// ---------------------------------------------------------------------------

#[test]
fn test_record_store_single_and_multiple_records() {
    let (mut pool, _dir, _path) = create_test_pool(10);
    let mut rs = RecordStore::new();

    let rid1 = rs.insert(&mut pool, b"record one").expect("insert 1");
    let rid2 = rs.insert(&mut pool, b"record two").expect("insert 2");
    let rid3 = rs.insert(&mut pool, b"record three").expect("insert 3");

    assert_eq!(
        rs.get(&mut pool, rid1).unwrap(),
        Some(b"record one".to_vec())
    );
    assert_eq!(
        rs.get(&mut pool, rid2).unwrap(),
        Some(b"record two".to_vec())
    );
    assert_eq!(
        rs.get(&mut pool, rid3).unwrap(),
        Some(b"record three".to_vec())
    );

    // Delete rid2
    assert!(rs.delete(&mut pool, rid2).unwrap());
    assert_eq!(rs.get(&mut pool, rid2).unwrap(), None);
    assert_eq!(
        rs.get(&mut pool, rid1).unwrap(),
        Some(b"record one".to_vec())
    );
    assert_eq!(
        rs.get(&mut pool, rid3).unwrap(),
        Some(b"record three".to_vec())
    );
}

#[test]
fn test_record_store_multi_page_allocation() {
    let (mut pool, _dir, _path) = create_test_pool(15);
    let mut rs = RecordStore::new();

    // Insert 10 records of 1000 bytes each -> requires multiple pages (since max page payload ~4082)
    let payload = vec![0xFEu8; 1000];
    let mut rids = Vec::new();

    for i in 0..10 {
        let mut data = payload.clone();
        data[0] = i as u8;
        let rid = rs.insert(&mut pool, &data).expect("insert multi-page");
        rids.push((rid, data));
    }

    // Verify multiple pages were allocated
    let distinct_pages: std::collections::HashSet<PageId> =
        rids.iter().map(|(rid, _)| rid.page_id).collect();
    assert!(
        distinct_pages.len() >= 3,
        "expected at least 3 record pages for 10 KB of data"
    );

    // Verify all records retrieved accurately
    for (rid, expected) in &rids {
        let fetched = rs.get(&mut pool, *rid).unwrap().expect("fetched");
        assert_eq!(&fetched, expected);
    }
}

#[test]
fn test_record_store_invalid_record_id_handling() {
    let (mut pool, _dir, _path) = create_test_pool(5);
    let mut rs = RecordStore::new();

    // PageId 0 (INVALID_PAGE_ID)
    assert_eq!(rs.get(&mut pool, RecordId::new(0, 0)).unwrap(), None);
    assert!(!rs.delete(&mut pool, RecordId::new(0, 0)).unwrap());

    // Out-of-bounds slot
    let rid = rs.insert(&mut pool, b"valid").expect("insert");
    let invalid_slot_rid = RecordId::new(rid.page_id, 999);
    assert_eq!(rs.get(&mut pool, invalid_slot_rid).unwrap(), None);
    assert!(!rs.delete(&mut pool, invalid_slot_rid).unwrap());
}

#[test]
fn test_record_store_pin_discipline_and_no_pin_leaks() {
    let (mut pool, _dir, _path) = create_test_pool(10);
    let mut rs = RecordStore::new();

    let mut rids = Vec::new();
    for i in 0..50 {
        let data = format!("payload test {i}").into_bytes();
        let rid = rs.insert(&mut pool, &data).expect("insert");
        rids.push(rid);

        // Verify no pins leaked after insert
        for pid in 0..pool.page_count() {
            if pool.is_resident(pid) {
                assert_eq!(
                    pool.pin_count(pid).unwrap(),
                    0,
                    "page {pid} leaked a pin after insert {i}"
                );
            }
        }
    }

    // Verify no pins leaked after get
    for rid in &rids {
        let _ = rs.get(&mut pool, *rid).unwrap();
        for pid in 0..pool.page_count() {
            if pool.is_resident(pid) {
                assert_eq!(
                    pool.pin_count(pid).unwrap(),
                    0,
                    "page {pid} leaked a pin after get"
                );
            }
        }
    }

    // Verify no pins leaked after delete
    for rid in &rids {
        let _ = rs.delete(&mut pool, *rid).unwrap();
        for pid in 0..pool.page_count() {
            if pool.is_resident(pid) {
                assert_eq!(
                    pool.pin_count(pid).unwrap(),
                    0,
                    "page {pid} leaked a pin after delete"
                );
            }
        }
    }
}

#[test]
fn test_record_store_persistence_across_reopen() {
    let (mut pool, _dir, db_path) = create_test_pool(20);
    let mut rs = RecordStore::new();

    let mut stored = Vec::new();
    for i in 1..=100 {
        let data = format!("persistent value {i} - long text string padding").into_bytes();
        let rid = rs.insert(&mut pool, &data).expect("insert");
        stored.push((rid, data));
    }

    // Delete some records
    for i in (0..100).step_by(3) {
        let rid = stored[i].0;
        rs.delete(&mut pool, rid).expect("delete");
    }

    pool.sync().expect("sync pool");
    drop(pool);

    // Reopen database from disk
    let pm = PageManager::open(&db_path).expect("reopen pm");
    let mut new_pool = BufferPool::new(20, pm).expect("new pool");
    let reopened_rs = RecordStore::new();

    for (i, (rid, data)) in stored.iter().enumerate() {
        if i % 3 == 0 {
            assert_eq!(reopened_rs.get(&mut new_pool, *rid).unwrap(), None);
        } else {
            assert_eq!(
                reopened_rs.get(&mut new_pool, *rid).unwrap(),
                Some(data.clone())
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Deterministic Randomized Testing with Oracle
// ---------------------------------------------------------------------------

#[test]
fn test_randomized_record_store_oracle() {
    let (mut pool, _dir, _path) = create_test_pool(25);
    let mut rs = RecordStore::new();
    let mut oracle: HashMap<RecordId, Vec<u8>> = HashMap::new();
    let mut live_rids: Vec<RecordId> = Vec::new();

    let mut rng_state: u64 = 0xCAFE_BABE_DEAD_BEEF;
    let mut next_rand = || -> u64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    let total_operations = 1000;

    for step in 1..=total_operations {
        let op_type = next_rand() % 10;

        if op_type < 6 || live_rids.is_empty() {
            // 60% chance: Insert variable-length record
            let len = (next_rand() % 300) as usize + 1;
            let byte_val = (step % 255) as u8;
            let data = vec![byte_val; len];

            let rid = rs.insert(&mut pool, &data).expect("insert");
            oracle.insert(rid, data);
            live_rids.push(rid);
        } else if op_type < 8 {
            // 20% chance: Read random existing record
            let idx = (next_rand() as usize) % live_rids.len();
            let rid = live_rids[idx];
            let actual = rs.get(&mut pool, rid).unwrap();
            let expected = oracle.get(&rid).cloned();
            assert_eq!(actual, expected, "step {step}: data mismatch on get");
        } else {
            // 20% chance: Delete random record
            let idx = (next_rand() as usize) % live_rids.len();
            let rid = live_rids.swap_remove(idx);
            let deleted = rs.delete(&mut pool, rid).unwrap();
            assert!(deleted, "step {step}: delete returned false");
            oracle.remove(&rid);
        }

        // Periodic full consistency check
        if step % 200 == 0 || step == total_operations {
            for (&rid, expected) in &oracle {
                let actual = rs.get(&mut pool, rid).unwrap().expect("record found");
                assert_eq!(&actual, expected, "step {step}: oracle mismatch");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. High-Level TreeDb Database Integration Tests
// ---------------------------------------------------------------------------

#[test]
fn test_treedb_put_get_delete_basic() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("treedb_basic.treedb");

    let mut db = TreeDb::create(&db_path, 10).expect("create treedb");

    db.put(10, b"alice").unwrap();
    db.put(20, b"bob").unwrap();
    db.put(30, b"charlie").unwrap();

    assert_eq!(db.get(10).unwrap(), Some(b"alice".to_vec()));
    assert_eq!(db.get(20).unwrap(), Some(b"bob".to_vec()));
    assert_eq!(db.get(30).unwrap(), Some(b"charlie".to_vec()));
    assert_eq!(db.get(40).unwrap(), None);

    // Delete key 20
    assert_eq!(db.delete(20).unwrap(), Some(b"bob".to_vec()));
    assert_eq!(db.get(20).unwrap(), None);
    assert_eq!(db.get(10).unwrap(), Some(b"alice".to_vec()));
    assert_eq!(db.get(30).unwrap(), Some(b"charlie".to_vec()));

    // Overwrite key 10
    db.put(10, b"alice_updated").unwrap();
    assert_eq!(db.get(10).unwrap(), Some(b"alice_updated".to_vec()));
}

#[test]
fn test_treedb_range_scan() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("treedb_scan.treedb");

    let mut db = TreeDb::create(&db_path, 10).expect("create treedb");

    for i in 1..=20 {
        let key = (i * 10) as u64;
        let val = format!("value_{key}").into_bytes();
        db.put(key, &val).unwrap();
    }

    // Scan [50, 150) -> 50, 60, ..., 140
    let res = db.scan(Some(50), Some(150)).unwrap();
    let scanned_keys: Vec<u64> = res.iter().map(|(k, _)| *k).collect();
    let expected_keys: Vec<u64> = (5..15).map(|i| (i * 10) as u64).collect();
    assert_eq!(scanned_keys, expected_keys);

    for (k, val) in res {
        assert_eq!(val, format!("value_{k}").into_bytes());
    }
}

#[test]
fn test_treedb_large_dataset_and_persistence() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("treedb_large.treedb");

    let count = 500;
    {
        let mut db = TreeDb::create(&db_path, 30).expect("create db");
        for i in 1..=count {
            let key = (i * 3) as u64;
            let val = format!("payload for key {key} - variable length data").into_bytes();
            db.put(key, &val).expect("put");
        }

        // Delete even keys
        for i in (2..=count).step_by(2) {
            let key = (i * 3) as u64;
            let expected_val = format!("payload for key {key} - variable length data").into_bytes();
            assert_eq!(db.delete(key).unwrap(), Some(expected_val));
        }

        db.sync().expect("sync");
    }

    // Reopen database from disk
    {
        let mut db = TreeDb::open(&db_path, 30).expect("reopen db");

        for i in 1..=count {
            let key = (i * 3) as u64;
            if i % 2 == 0 {
                assert_eq!(db.get(key).unwrap(), None);
            } else {
                let expected = format!("payload for key {key} - variable length data").into_bytes();
                assert_eq!(db.get(key).unwrap(), Some(expected));
            }
        }
    }
}
