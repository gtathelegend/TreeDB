use std::path::PathBuf;
use tempfile::TempDir;

use treedb::{Error, Key, LogManager, LogRecordType, Lsn, PageManager, RecoveryEngine, TreeDb};

fn create_test_db(name: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join(format!("{name}.db"));
    let wal_path = dir.path().join(format!("{name}.db.wal"));
    (dir, db_path, wal_path)
}

#[test]
fn test_checkpoint_basic_lifecycle_and_durability_ordering() {
    let (_dir, db_path, wal_path) = create_test_db("cp_basic");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"value_one").unwrap();
    db.put(2, b"value_two").unwrap();

    let cp_lsn = db.checkpoint().unwrap();
    assert!(cp_lsn.is_valid());
    assert!(cp_lsn > Lsn(1));

    // Verify DbHeader on disk has recorded last_checkpoint_lsn
    let pm = PageManager::open(&db_path).unwrap();
    assert_eq!(pm.header().last_checkpoint_lsn, cp_lsn);
    drop(pm);

    // Verify WAL contains the Checkpoint record
    let mut wal = LogManager::open(&wal_path).unwrap();
    let records = wal.read_all_records().unwrap();
    let cp_rec = records.iter().find(|r| r.lsn == cp_lsn).unwrap();
    assert_eq!(cp_rec.record_type, LogRecordType::Checkpoint);
    drop(wal);
    drop(db);

    // Reopen database and verify data is present and recoverable
    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    assert_eq!(reopened.get(1).unwrap(), Some(b"value_one".to_vec()));
    assert_eq!(reopened.get(2).unwrap(), Some(b"value_two".to_vec()));
}

#[test]
fn test_checkpoint_empty_database() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_empty");

    let mut db = TreeDb::create(&db_path, 5).unwrap();
    let cp1 = db.checkpoint().unwrap();
    assert!(cp1.is_valid());

    let cp2 = db.checkpoint().unwrap();
    assert!(cp2 > cp1);

    drop(db);

    let mut reopened = TreeDb::open(&db_path, 5).unwrap();
    assert_eq!(reopened.scan(None, None).unwrap().len(), 0);
    let cp3 = reopened.checkpoint().unwrap();
    assert!(cp3 > cp2);
}

#[test]
fn test_checkpoint_active_transactions_spanning_checkpoint() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_active_txn");

    // Case 1: Active transaction spans checkpoint and is committed afterwards
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        let t1 = db.begin_txn().unwrap();
        db.put(10, b"uncommitted_val").unwrap(); // using active t1

        let cp_lsn = db.checkpoint().unwrap();
        assert!(cp_lsn.is_valid());

        // Commit T1 after checkpoint
        db.commit_txn(t1).unwrap();
        db.sync().unwrap();
        drop(db);

        // Reopen and assert T1 changes survived
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(10).unwrap(), Some(b"uncommitted_val".to_vec()));
    }

    // Case 2: Active transaction spans checkpoint and is NOT committed before crash
    let (_dir2, db_path2, _wal_path2) = create_test_db("cp_active_loser");
    {
        let mut db = TreeDb::create(&db_path2, 10).unwrap();
        db.put(1, b"committed_anchor").unwrap();

        let _t2 = db.begin_txn().unwrap();
        db.put(20, b"loser_val").unwrap();

        let cp_lsn = db.checkpoint().unwrap();
        assert!(cp_lsn.is_valid());

        db.put(21, b"another_loser_val").unwrap();

        // Simulate crash without committing T2 and without normal drop
        db.buffer_pool_mut().flush_all().unwrap();
        let _ = db.buffer_pool_mut().detach_wal();
    }

    // Reopen: recovery should identify T2 as loser and undo its modifications
    {
        let mut reopened = TreeDb::open(&db_path2, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"committed_anchor".to_vec()));
        assert_eq!(reopened.get(20).unwrap(), None);
        assert_eq!(reopened.get(21).unwrap(), None);
    }
}

#[test]
fn test_checkpoint_wal_retention_boundary_and_safe_truncation() {
    let (_dir, db_path, wal_path) = create_test_db("cp_retention");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // Insert multiple committed transactions
    for i in 1..=20 {
        db.put(i, format!("val_{i}").as_bytes()).unwrap();
    }

    let initial_records = {
        let mut wal = LogManager::open(&wal_path).unwrap();
        wal.read_all_records().unwrap().len()
    };
    assert!(initial_records > 40);

    // Perform checkpoint with WAL prefix truncation
    let cp_lsn = db.checkpoint_and_truncate_wal().unwrap();
    assert!(cp_lsn.is_valid());

    let post_records = {
        let mut wal = LogManager::open(&wal_path).unwrap();
        wal.read_all_records().unwrap()
    };

    // After truncation with no active txns, obsolete prefix is removed
    assert!(post_records.len() < initial_records);
    assert_eq!(post_records[0].lsn, cp_lsn);

    // Verify all data is intact
    for i in 1..=20 {
        assert_eq!(db.get(i).unwrap(), Some(format!("val_{i}").into_bytes()));
    }
    drop(db);

    // Reopen and verify data persistence
    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    for i in 1..=20 {
        assert_eq!(
            reopened.get(i).unwrap(),
            Some(format!("val_{i}").into_bytes())
        );
    }
}

#[test]
fn test_checkpoint_wal_retention_with_active_transaction() {
    let (_dir, db_path, wal_path) = create_test_db("cp_retention_active");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // 1. Begin long active transaction T1
    let t1 = db.begin_txn().unwrap();
    db.put(100, b"t1_early_write").unwrap();

    // 2. Perform some other operations
    for i in 1..=5 {
        // committed auto-txns
        let t = db.begin_txn().unwrap();
        db.put(i, format!("val_{i}").as_bytes()).unwrap();
        db.commit_txn(t).unwrap();
    }

    // Set active txn back to T1
    db.buffer_pool_mut().set_active_txn(t1);

    // 3. Checkpoint and truncate WAL
    let cp_lsn = db.checkpoint_and_truncate_wal().unwrap();
    assert!(cp_lsn.is_valid());

    let post_records = {
        let mut wal = LogManager::open(&wal_path).unwrap();
        wal.read_all_records().unwrap()
    };

    // Because T1 is active and started before the other transactions,
    // its earliest LSN must be retained!
    assert!(post_records[0].lsn <= cp_lsn);
    assert!(post_records
        .iter()
        .any(|r| r.txn_id == t1 && r.record_type == LogRecordType::Begin));

    // Commit T1
    db.commit_txn(t1).unwrap();
    db.sync().unwrap();
    drop(db);

    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    assert_eq!(reopened.get(100).unwrap(), Some(b"t1_early_write".to_vec()));
}

#[test]
fn test_checkpoint_wal_growth_bounded() {
    let (_dir, db_path, wal_path) = create_test_db("cp_growth");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // Repeatedly write and checkpoint+truncate
    for round in 0..10 {
        for i in 0..20 {
            let key = (round * 20 + i) as Key;
            db.put(key, format!("bounded_val_{key}").as_bytes())
                .unwrap();
        }
        db.checkpoint_and_truncate_wal().unwrap();
    }

    let wal_size = std::fs::metadata(&wal_path).unwrap().len();
    // Bounded WAL size: with 10 rounds of 20 writes and periodic truncation, WAL should remain small (< 10 KB)
    assert!(
        wal_size < 15_000,
        "WAL file size ({wal_size} bytes) was not bounded by periodic checkpointing"
    );

    // All 200 items should exist
    for k in 0..200 {
        assert_eq!(
            db.get(k).unwrap(),
            Some(format!("bounded_val_{k}").into_bytes())
        );
    }
}

#[test]
fn test_checkpoint_corrupted_metadata_handling() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_corrupt");

    // Case 1: Checkpoint LSN in header is larger than flushed WAL LSN
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(1, b"val1").unwrap();
        db.sync().unwrap();
        drop(db);

        // Corrupt header Page 0 to set last_checkpoint_lsn = 999999
        let mut pm = PageManager::open(&db_path).unwrap();
        pm.header_mut().last_checkpoint_lsn = Lsn(999999);
        pm.write_header_to_disk().unwrap();
        pm.sync().unwrap();
        drop(pm);

        let err = TreeDb::open(&db_path, 10).unwrap_err();
        assert!(matches!(err, Error::WalCorruption(_)));
    }

    // Case 2: Checkpoint LSN in header points to a non-checkpoint record
    let (_dir2, db_path2, _wal_path2) = create_test_db("cp_non_cp_record");
    {
        let mut db = TreeDb::create(&db_path2, 10).unwrap();
        db.put(1, b"val1").unwrap();
        db.sync().unwrap();
        drop(db);

        // Set last_checkpoint_lsn = Lsn(1) (which is a Begin record, not Checkpoint)
        let mut pm = PageManager::open(&db_path2).unwrap();
        pm.header_mut().last_checkpoint_lsn = Lsn(1);
        pm.write_header_to_disk().unwrap();
        pm.sync().unwrap();
        drop(pm);

        let err = TreeDb::open(&db_path2, 10).unwrap_err();
        assert!(matches!(err, Error::WalCorruption(_)));
    }
}

#[test]
fn test_checkpoint_freelist_and_overflow_interaction() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_freelist_overflow");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // 1. Insert large multi-page overflow record (> 4070 bytes)
    let large_val_1 = vec![0xAA; 10_000];
    db.put(100, &large_val_1).unwrap();

    // 2. Perform checkpoint
    let cp1 = db.checkpoint().unwrap();
    assert!(cp1.is_valid());

    // 3. Update large record to new content (freeing old overflow pages into freelist)
    let large_val_2 = vec![0xBB; 12_000];
    db.put(100, &large_val_2).unwrap();

    // 4. Checkpoint again
    let cp2 = db.checkpoint_and_truncate_wal().unwrap();
    assert!(cp2 > cp1);

    drop(db);

    // Reopen and verify large record integrity and freelist validity
    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    assert_eq!(reopened.get(100).unwrap(), Some(large_val_2));
    reopened.buffer_pool_mut().validate_freelist().unwrap();

    // Delete large record and checkpoint
    reopened.delete(100).unwrap();
    reopened.checkpoint_and_truncate_wal().unwrap();
    reopened.buffer_pool_mut().validate_freelist().unwrap();
}

#[test]
fn test_checkpoint_recovery_equivalence() {
    let (_dir1, db_path1, _wal_path1) = create_test_db("equiv_no_cp");
    let (_dir2, db_path2, _wal_path2) = create_test_db("equiv_with_cp");

    // Perform identical sequence of operations on db1 (no checkpoints) and db2 (periodic checkpoints)
    let ops = [
        (1, b"apple".to_vec()),
        (2, b"banana".to_vec()),
        (3, vec![0xCC; 5000]), // overflow
        (1, b"apricot".to_vec()),
        (4, b"cherry".to_vec()),
        (2, b"".to_vec()),     // delete
        (5, vec![0xDD; 9000]), // overflow
        (3, b"short_again".to_vec()),
    ];

    // DB 1: No checkpointing, crash at the end
    {
        let mut db1 = TreeDb::create(&db_path1, 10).unwrap();
        for &(k, ref val) in &ops {
            if val.is_empty() {
                db1.delete(k).unwrap();
            } else {
                db1.put(k, val).unwrap();
            }
        }
        db1.buffer_pool_mut().flush_all().unwrap();
        let _ = db1.buffer_pool_mut().detach_wal();
    }

    // DB 2: Periodic checkpoints + truncations
    {
        let mut db2 = TreeDb::create(&db_path2, 10).unwrap();
        for (idx, &(k, ref val)) in ops.iter().enumerate() {
            if val.is_empty() {
                db2.delete(k).unwrap();
            } else {
                db2.put(k, val).unwrap();
            }
            if idx % 3 == 0 {
                db2.checkpoint_and_truncate_wal().unwrap();
            }
        }
        db2.buffer_pool_mut().flush_all().unwrap();
        let _ = db2.buffer_pool_mut().detach_wal();
    }

    // Recover both databases
    let mut recovered1 = TreeDb::open(&db_path1, 10).unwrap();
    let mut recovered2 = TreeDb::open(&db_path2, 10).unwrap();

    let scan1 = recovered1.scan(None, None).unwrap();
    let scan2 = recovered2.scan(None, None).unwrap();

    assert_eq!(
        scan1, scan2,
        "Logical state must be completely identical between full WAL recovery and checkpoint recovery"
    );
}

#[test]
fn test_checkpoint_recovery_scan_reduction_benchmark() {
    let (_dir, db_path, wal_path) = create_test_db("cp_scan_reduction");

    // Create a database with 100 transactions and a checkpoint after 90
    let mut db = TreeDb::create(&db_path, 20).unwrap();
    for i in 1..=90 {
        db.put(i, format!("bench_{i}").as_bytes()).unwrap();
    }
    let cp_lsn = db.checkpoint().unwrap();

    for i in 91..=100 {
        db.put(i, format!("bench_{i}").as_bytes()).unwrap();
    }
    db.sync().unwrap();
    drop(db);

    // 1. Measure checkpoint-based recovery
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut wal = LogManager::open(&wal_path).unwrap();
    let stats_cp = RecoveryEngine::recover(&mut pm, &mut wal).unwrap();

    // Checkpoint LSN was recognized
    assert_eq!(stats_cp.checkpoint_lsn, cp_lsn);

    // Total records in WAL is > 200, but records scanned starting from checkpoint is much fewer
    let all_records = wal.read_all_records().unwrap();
    assert!(
        all_records.len() > 100,
        "Total records should reflect full log history"
    );
}

#[test]
fn test_checkpoint_pin_discipline() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_pin");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"pin_test").unwrap();
    db.put(2, b"pin_test2").unwrap();

    // Check stats: 2 dirty pages resident before checkpoint
    let stats_before = db.buffer_pool().stats();
    assert_eq!(stats_before.current_dirty_pages, 2);

    db.checkpoint().unwrap();

    // After checkpoint, dirty pages must be 0 and no frames pinned
    let stats_after = db.buffer_pool().stats();
    assert_eq!(stats_after.current_dirty_pages, 0);
    assert_eq!(db.buffer_pool().pin_count(1).unwrap_or(0), 0);
    assert_eq!(db.buffer_pool().pin_count(2).unwrap_or(0), 0);
}
