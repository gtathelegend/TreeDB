use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

use treedb::engine::failure::StorageFailurePoint;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::engine::vacuum::{VacuumConfig, VacuumEngine};
use treedb::{BufferPool, PageManager, RecordStore, TreeDb};

fn make_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("payload", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn create_test_db(name: &str) -> (TreeDb, tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir, db_path)
}

fn reopen_db(path: &Path) -> TreeDb {
    TreeDb::open(path, 64).unwrap()
}

fn create_test_pool(capacity: usize) -> (BufferPool, tempfile::TempDir, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("test_pool.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");
    let pool = BufferPool::new(capacity, pm).expect("create buffer pool");
    (pool, dir, db_path)
}

fn reopen_pool(path: &Path, capacity: usize) -> BufferPool {
    let pm = PageManager::open(path).expect("open pm");
    BufferPool::new(capacity, pm).expect("reopen buffer pool")
}

fn make_row(id: i64, name: &str, payload: Vec<u8>) -> Row {
    Row::new(vec![
        Value::Int64(id),
        Value::String(name.to_string()),
        Value::Bytes(payload),
    ])
}

fn assert_db_consistency(db: &mut TreeDb) {
    db.validate_freelist().expect("freelist must be valid");
    VacuumEngine::validate_db_vacuum_consistency(db).expect("vacuum consistency validation failed");
}

// -----------------------------------------------------------------------------
// 1. Deletion of inline records vs overflow records
// -----------------------------------------------------------------------------
#[test]
fn test_deletion_inline_and_overflow_records() {
    let (mut pool, _dir, path) = create_test_pool(32);
    let mut store = RecordStore::new();

    let payload_inline = vec![42; 64];
    let payload_5k = vec![77; 5_000];
    let payload_10k = vec![99; 10_000];
    let rid_inline = store.insert(&mut pool, &payload_inline).unwrap();
    let rid_5k = store.insert(&mut pool, &payload_5k).unwrap();
    let rid_10k = store.insert(&mut pool, &payload_10k).unwrap();

    let initial_free = pool.page_manager().header().free_page_count;

    // Delete inline record
    assert!(store.delete(&mut pool, rid_inline).unwrap());
    assert_eq!(pool.page_manager().header().free_page_count, initial_free);

    // Delete 5KB overflow record
    assert!(store.delete(&mut pool, rid_5k).unwrap());
    let free_after_5k = pool.page_manager().header().free_page_count;
    assert!(free_after_5k > initial_free);

    // Delete 10KB overflow record
    assert!(store.delete(&mut pool, rid_10k).unwrap());
    let free_after_10k = pool.page_manager().header().free_page_count;
    assert!(free_after_10k > free_after_5k);

    pool.validate_freelist().unwrap();

    drop(pool);
    let mut reopened = reopen_pool(&path, 32);
    reopened.validate_freelist().unwrap();
    assert_eq!(
        reopened.page_manager().header().free_page_count,
        free_after_10k
    );
}

// -----------------------------------------------------------------------------
// 2. Very large overflow records (multi-page chain)
// -----------------------------------------------------------------------------
#[test]
fn test_deletion_very_large_overflow_chain() {
    let (mut pool, _dir, path) = create_test_pool(64);
    let mut store = RecordStore::new();

    let large_size = 25_000;
    let payload = vec![0xAB; large_size];
    let rid = store.insert(&mut pool, &payload).unwrap();

    let free_before = pool.page_manager().header().free_page_count;
    assert!(store.delete(&mut pool, rid).unwrap());

    let free_after = pool.page_manager().header().free_page_count;
    let freed_pages = free_after - free_before;
    assert!(
        freed_pages >= 6,
        "Expected at least 6 overflow pages freed, got {freed_pages}"
    );

    pool.validate_freelist().unwrap();

    drop(pool);
    let mut reopened = reopen_pool(&path, 64);
    reopened.validate_freelist().unwrap();
}

// -----------------------------------------------------------------------------
// 3. Repeated deletion attempts (idempotence)
// -----------------------------------------------------------------------------
#[test]
fn test_repeated_deletion_idempotence() {
    let (mut db, _dir, _path) = create_test_db("repeat_del");
    db.create_table("items", make_test_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&make_row(1, "item1", vec![1; 6000])).unwrap();
    }
    t0.commit().unwrap();

    // First delete succeeds
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("items").unwrap();
        assert!(tbl.delete(&Value::Int64(1)).unwrap().is_some());
    }
    t1.commit().unwrap();

    let free_count = db.free_page_count();

    // Second delete on non-existent row returns None without altering freelist
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("items").unwrap();
        assert!(tbl.delete(&Value::Int64(1)).unwrap().is_none());
    }
    t2.commit().unwrap();

    assert_eq!(db.free_page_count(), free_count);
    assert_db_consistency(&mut db);
}

// -----------------------------------------------------------------------------
// 4. Aborted deletion restores previous state
// -----------------------------------------------------------------------------
#[test]
fn test_aborted_deletion_restores_freelist_and_data() {
    let (mut db, _dir, path) = create_test_db("abort_deletion");
    db.create_table("items", make_test_schema()).unwrap();

    let payload = vec![0xEE; 8_000];
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&make_row(1, "overflow", payload.clone()))
            .unwrap();
    }
    t0.commit().unwrap();

    let initial_free_count = db.free_page_count();
    let initial_free_head = db.buffer_pool().page_manager().header().freelist_head;

    // Begin transaction, delete, then abort
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("items").unwrap();
        assert!(tbl.delete(&Value::Int64(1)).unwrap().is_some());
    }
    t1.abort().unwrap();

    // Freelist metadata must be restored
    assert_eq!(db.free_page_count(), initial_free_count);
    assert_eq!(
        db.buffer_pool().page_manager().header().freelist_head,
        initial_free_head
    );

    // Data must remain accessible
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("items").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().expect("row must exist");
        assert_eq!(row.values[2], Value::Bytes(payload.clone()));
    }
    t2.commit().unwrap();

    assert_db_consistency(&mut db);

    // Reopen and verify persistence
    drop(db);
    let mut reopened = reopen_db(&path);
    assert_db_consistency(&mut reopened);
    let mut t3 = reopened.begin().unwrap();
    {
        let mut tbl = t3.open_table("items").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().expect("row must exist");
        assert_eq!(row.values[2], Value::Bytes(payload));
    }
    t3.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 5. Reuse of reclaimed pages (LIFO allocation)
// -----------------------------------------------------------------------------
#[test]
fn test_reuse_of_reclaimed_overflow_pages() {
    let (mut pool, _dir, _path) = create_test_pool(64);
    let mut store = RecordStore::new();

    let payload1 = vec![0x11; 8_000];
    let rid = store.insert(&mut pool, &payload1).unwrap();
    let page_count_before = pool.page_count();

    assert!(store.delete(&mut pool, rid).unwrap());
    assert!(pool.page_manager().header().free_page_count > 0);

    let payload2 = vec![0x22; 8_000];
    let rid2 = store.insert(&mut pool, &payload2).unwrap();
    let page_count_after = pool.page_count();
    assert_eq!(
        page_count_before, page_count_after,
        "Page count should not grow when recyclable pages exist in freelist"
    );

    let val = store.get(&mut pool, rid2).unwrap().unwrap();
    assert_eq!(val, vec![0x22; 8_000]);

    pool.validate_freelist().unwrap();
}

// -----------------------------------------------------------------------------
// 6. Failure during freelist mutation (Crash Simulation)
// -----------------------------------------------------------------------------
#[test]
fn test_failure_during_freelist_update_and_recovery() {
    let (mut db, _dir, path) = create_test_db("fail_freelist_mut");
    db.create_table("items", make_test_schema()).unwrap();

    let payload = vec![0x99; 9_000];
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&make_row(1, "target", payload.clone())).unwrap();
    }
    t0.commit().unwrap();

    // Injected crash during freelist update on delete
    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::DuringFreelistUpdate));
    {
        let mut tbl = t1.open_table("items").unwrap();
        let _ = tbl.delete(&Value::Int64(1));
    }
    std::mem::forget(t1);
    drop(db);

    // Reopen and trigger ARIES recovery
    let mut reopened = reopen_db(&path);
    assert_db_consistency(&mut reopened);

    // The uncommitted deletion must be rolled back: row remains visible
    let mut t2 = reopened.begin().unwrap();
    {
        let mut tbl = t2.open_table("items").unwrap();
        let row = tbl
            .get(&Value::Int64(1))
            .unwrap()
            .expect("row must survive uncommitted crash");
        assert_eq!(row.values[2], Value::Bytes(payload));
    }
    t2.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 7. MVCC snapshot isolation with overflow chains
// -----------------------------------------------------------------------------
#[test]
fn test_mvcc_snapshot_isolation_with_overflow_records() {
    let (mut db, _dir, _path) = create_test_db("mvcc_overflow_snap");
    db.create_table("items", make_test_schema()).unwrap();

    let v1 = vec![0x11; 6_000];
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&make_row(1, "v1", v1.clone())).unwrap();
    }
    t0.commit().unwrap();

    // Long-running read snapshot
    let s1 = db.register_snapshot();

    // Update row to version 2 (different overflow payload)
    let v2 = vec![0x22; 7_000];
    let mut writer = db.begin().unwrap();
    {
        let mut tbl = writer.open_table("items").unwrap();
        tbl.update(&make_row(1, "v2", v2.clone())).unwrap();
    }
    writer.commit().unwrap();

    // Reader with snapshot s1 must still observe v1 despite v2 being committed
    {
        let mut reader = db.begin().unwrap();
        reader.set_snapshot(s1);
        let mut tbl = reader.open_table("items").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().expect("row v1");
        assert_eq!(row.values[2], Value::Bytes(v1));
        reader.commit().unwrap();
    }
    db.release_snapshot(s1.snapshot_id);

    // Subsequent reader observes v2
    let mut new_reader = db.begin().unwrap();
    {
        let mut tbl = new_reader.open_table("items").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().expect("row v2");
        assert_eq!(row.values[2], Value::Bytes(v2));
    }
    new_reader.commit().unwrap();

    assert_db_consistency(&mut db);
}

// -----------------------------------------------------------------------------
// 8. Vacuum reclamation of obsolete overflow versions
// -----------------------------------------------------------------------------
#[test]
fn test_vacuum_reclaims_obsolete_overflow_versions() {
    let (mut db, _dir, _path) = create_test_db("vacuum_overflow");
    db.create_table("items", make_test_schema()).unwrap();

    // Insert initial overflow row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&make_row(1, "orig", vec![1; 7_000])).unwrap();
    }
    t0.commit().unwrap();

    // Update 3 times with new overflow payloads
    for r in 2..=4 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("items").unwrap();
            tbl.update(&make_row(1, &format!("v{r}"), vec![r as u8; 7_000]))
                .unwrap();
        }
        t.commit().unwrap();
    }

    let free_before_vacuum = db.free_page_count();

    // Run online vacuum: obsolete historical versions should be pruned and overflow pages freed
    let vresult = db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(
        vresult.versions_reclaimed >= 3,
        "Expected at least 3 historical versions reclaimed, got {}",
        vresult.versions_reclaimed
    );
    assert!(
        vresult.overflow_pages_reclaimed >= 3,
        "Expected at least 3 overflow pages reclaimed, got {}",
        vresult.overflow_pages_reclaimed
    );

    let free_after_vacuum = db.free_page_count();
    assert!(
        free_after_vacuum > free_before_vacuum,
        "Vacuum should increase free page count"
    );

    // Latest version remains visible
    let mut t_check = db.begin().unwrap();
    {
        let mut tbl = t_check.open_table("items").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().expect("v4 must exist");
        assert_eq!(row.values[1], Value::String("v4".to_string()));
    }
    t_check.commit().unwrap();

    assert_db_consistency(&mut db);
}

// -----------------------------------------------------------------------------
// 9. Differential Randomized Oracle Validation with Fixed Seed
// -----------------------------------------------------------------------------
#[test]
fn test_differential_randomized_overflow_oracle() {
    let (mut db, _dir, path) = create_test_db("rand_oracle_diff");
    db.create_table("items", make_test_schema()).unwrap();

    let mut oracle: HashMap<i64, (String, Vec<u8>)> = HashMap::new();
    let mut rng = 0x4D59_5345_4544_3832u64;
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let payload_sizes = [
        64,     // small inline
        512,    // medium inline
        3_500,  // near max inline
        4_200,  // 1-page overflow
        8_500,  // 2-page overflow
        14_000, // 4-page overflow
    ];

    let total_ops = 150;
    for op in 0..total_ops {
        let op_choice = next_rand() % 10;
        let key = ((next_rand() % 30) + 1) as i64;

        match op_choice {
            0..=4 => {
                // Insert or Update
                let size = payload_sizes[(next_rand() as usize) % payload_sizes.len()];
                let mut bytes = vec![0u8; size];
                for b in &mut bytes {
                    *b = (next_rand() % 256) as u8;
                }
                let name = format!("name_{key}_{op}");
                let row = make_row(key, &name, bytes.clone());

                let mut txn = db.begin().unwrap();
                {
                    let mut tbl = txn.open_table("items").unwrap();
                    if oracle.contains_key(&key) {
                        tbl.update(&row).unwrap();
                    } else {
                        tbl.insert(&row).unwrap();
                    }
                }
                txn.commit().unwrap();
                oracle.insert(key, (name, bytes));
            }
            5..=6 => {
                // Delete
                let mut txn = db.begin().unwrap();
                let actual_deleted = {
                    let mut tbl = txn.open_table("items").unwrap();
                    tbl.delete(&Value::Int64(key)).unwrap().is_some()
                };
                txn.commit().unwrap();

                let expected_deleted = oracle.remove(&key).is_some();
                assert_eq!(actual_deleted, expected_deleted);
            }
            7 => {
                // Aborted delete attempt
                if oracle.contains_key(&key) {
                    let mut txn = db.begin().unwrap();
                    {
                        let mut tbl = txn.open_table("items").unwrap();
                        let _ = tbl.delete(&Value::Int64(key)).unwrap();
                    }
                    txn.abort().unwrap();
                    // Oracle unchanged
                }
            }
            8..=9 => {
                // Query point lookup
                let mut txn = db.begin().unwrap();
                {
                    let mut tbl = txn.open_table("items").unwrap();
                    let actual = tbl.get(&Value::Int64(key)).unwrap();
                    match oracle.get(&key) {
                        Some((expected_name, expected_bytes)) => {
                            let row = actual.expect("row must exist");
                            assert_eq!(row.values[1], Value::String(expected_name.clone()));
                            assert_eq!(row.values[2], Value::Bytes(expected_bytes.clone()));
                        }
                        None => {
                            assert!(actual.is_none());
                        }
                    }
                }
                txn.commit().unwrap();
            }
            _ => unreachable!(),
        }

        if op % 30 == 0 {
            db.vacuum(&VacuumConfig::default()).unwrap();
            assert_db_consistency(&mut db);
        }
    }

    db.sync().unwrap();
    assert_db_consistency(&mut db);

    // Reopen and check entire oracle
    drop(db);
    let mut reopened = reopen_db(&path);
    assert_db_consistency(&mut reopened);

    let mut final_txn = reopened.begin().unwrap();
    {
        let mut tbl = final_txn.open_table("items").unwrap();
        for (&key, (expected_name, expected_bytes)) in &oracle {
            let row = tbl
                .get(&Value::Int64(key))
                .unwrap()
                .expect("row on final reopen");
            assert_eq!(row.values[1], Value::String(expected_name.clone()));
            assert_eq!(row.values[2], Value::Bytes(expected_bytes.clone()));
        }
    }
    final_txn.commit().unwrap();
}
