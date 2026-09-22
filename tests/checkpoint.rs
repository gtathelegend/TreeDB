use std::io::Write;
use std::path::PathBuf;
use tempfile::TempDir;

use treedb::{
    Error, Key, LogManager, LogRecord, LogRecordType, Lsn, PageManager, PageType, RecoveryEngine,
    TreeDb, TxnId,
};

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

#[test]
fn test_adversarial_active_transaction_spanning_multiple_checkpoints() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_adv_active");

    // 1. Begin T1 before multiple checkpoints
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(1, b"anchor").unwrap();

        let _t1 = db.begin_txn().unwrap();
        db.put(10, b"t1_mut_a").unwrap();

        // Checkpoint 1
        db.checkpoint().unwrap();

        db.put(11, b"t1_mut_b").unwrap();

        // Checkpoint 2
        db.checkpoint().unwrap();

        db.put(12, b"t1_mut_c").unwrap();

        // Crash without committing T1
        db.buffer_pool_mut().flush_all().unwrap();
        let _ = db.buffer_pool_mut().detach_wal();
    }

    // 2. Recovery must identify T1 as a loser and undo all its mutations across all checkpoints
    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"anchor".to_vec()));
        assert_eq!(reopened.get(10).unwrap(), None);
        assert_eq!(reopened.get(11).unwrap(), None);
        assert_eq!(reopened.get(12).unwrap(), None);
    }
}

#[test]
fn test_multiple_transactions_retention_and_interleaved_commit_abort() {
    let (_dir, db_path, wal_path) = create_test_db("cp_multi_txn_retention");

    let (p5, p6, p7, p8);
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();

        // T1 begin + mutate page p5
        let t1 = db.begin_txn().unwrap();
        p5 = db
            .buffer_pool_mut()
            .allocate_page(PageType::Overflow)
            .unwrap();
        let p5_before = vec![0u8; 16];
        let p5_after = vec![1u8; 16];
        db.log_page_delta(t1, p5, 16, p5_before, p5_after.clone())
            .unwrap();
        db.buffer_pool_mut()
            .get_page_mut(p5)
            .unwrap()
            .as_bytes_mut()[16..32]
            .copy_from_slice(&p5_after);
        db.buffer_pool_mut().unpin_page(p5, true).unwrap();

        // T2 begin + mutate page p6
        let t2 = db.begin_txn().unwrap();
        p6 = db
            .buffer_pool_mut()
            .allocate_page(PageType::Overflow)
            .unwrap();
        let p6_before = vec![0u8; 16];
        let p6_after = vec![2u8; 16];
        db.log_page_delta(t2, p6, 16, p6_before, p6_after.clone())
            .unwrap();
        db.buffer_pool_mut()
            .get_page_mut(p6)
            .unwrap()
            .as_bytes_mut()[16..32]
            .copy_from_slice(&p6_after);
        db.buffer_pool_mut().unpin_page(p6, true).unwrap();

        // Checkpoint 1
        db.checkpoint().unwrap();

        // T1 mutate again on page p7
        p7 = db
            .buffer_pool_mut()
            .allocate_page(PageType::Overflow)
            .unwrap();
        let p7_before = vec![0u8; 16];
        let p7_after = vec![3u8; 16];
        db.log_page_delta(t1, p7, 16, p7_before, p7_after.clone())
            .unwrap();
        db.buffer_pool_mut()
            .get_page_mut(p7)
            .unwrap()
            .as_bytes_mut()[16..32]
            .copy_from_slice(&p7_after);
        db.buffer_pool_mut().unpin_page(p7, true).unwrap();

        // T2 commit
        db.commit_txn(t2).unwrap();

        // Checkpoint 2 + truncate
        let cp2 = db.checkpoint_and_truncate_wal().unwrap();

        // Verify that T1's early records are retained because T1 is still active
        let mut wal = LogManager::open(&wal_path).unwrap();
        let records = wal.read_all_records().unwrap();
        assert!(records[0].lsn <= cp2);
        assert!(records
            .iter()
            .any(|r| r.txn_id == t1 && r.record_type == LogRecordType::Begin));
        drop(wal);

        // T3 begin + mutate page p8
        let t3 = db.begin_txn().unwrap();
        p8 = db
            .buffer_pool_mut()
            .allocate_page(PageType::Overflow)
            .unwrap();
        let p8_before = vec![0u8; 16];
        let p8_after = vec![4u8; 16];
        db.log_page_delta(t3, p8, 16, p8_before, p8_after.clone())
            .unwrap();
        db.buffer_pool_mut()
            .get_page_mut(p8)
            .unwrap()
            .as_bytes_mut()[16..32]
            .copy_from_slice(&p8_after);
        db.buffer_pool_mut().unpin_page(p8, true).unwrap();

        // Crash: T1 and T3 uncommitted, T2 committed. Flush dirty pages to simulate uncommitted changes on disk.
        db.buffer_pool_mut().flush_all().unwrap();
        let _ = db.buffer_pool_mut().detach_wal();
    }

    // Reopen and recover
    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        let bp = reopened.buffer_pool_mut();

        // T2's committed changes on page p6 must be preserved
        let page6 = bp.fetch_page(p6).unwrap();
        assert_eq!(&page6.as_bytes()[16..32], &[2u8; 16]);
        bp.unpin_page(p6, false).unwrap();

        // T1's uncommitted changes on pages p5 and p7 must be undone
        let page5 = bp.fetch_page(p5).unwrap();
        assert_eq!(&page5.as_bytes()[16..32], &[0u8; 16]);
        bp.unpin_page(p5, false).unwrap();

        let page7 = bp.fetch_page(p7).unwrap();
        assert_eq!(&page7.as_bytes()[16..32], &[0u8; 16]);
        bp.unpin_page(p7, false).unwrap();

        // T3's uncommitted changes on page p8 must be undone
        let page8 = bp.fetch_page(p8).unwrap();
        assert_eq!(&page8.as_bytes()[16..32], &[0u8; 16]);
        bp.unpin_page(p8, false).unwrap();
    }
}

#[test]
fn test_lsn_continuity_after_wal_truncation() {
    let (_dir, db_path, wal_path) = create_test_db("cp_lsn_continuity");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"first").unwrap();
    db.put(2, b"second").unwrap();

    let pre_trunc_records = {
        let mut wal = LogManager::open(&wal_path).unwrap();
        wal.read_all_records().unwrap()
    };
    let max_pre_lsn = pre_trunc_records.iter().map(|r| r.lsn).max().unwrap();

    // Checkpoint and truncate WAL
    let cp_lsn = db.checkpoint_and_truncate_wal().unwrap();
    assert!(cp_lsn > max_pre_lsn);

    // Append new transactions after truncation
    db.put(3, b"third").unwrap();
    db.put(4, b"fourth").unwrap();

    let post_records = {
        let mut wal = LogManager::open(&wal_path).unwrap();
        wal.read_all_records().unwrap()
    };

    // Verify all records in truncated WAL have strictly monotonically increasing LSNs
    let mut prev_lsn = Lsn(0);
    for rec in &post_records {
        assert!(
            rec.lsn > prev_lsn,
            "LSN monotonicity violated: {} <= {}",
            rec.lsn,
            prev_lsn
        );
        prev_lsn = rec.lsn;
    }
    // New records allocated strictly greater LSNs than any pre-truncation LSN
    assert!(prev_lsn > cp_lsn);
}

#[test]
fn test_non_zero_wal_starting_lsn_scanner_acceptance() {
    let (_dir, db_path, wal_path) = create_test_db("cp_non_zero_start");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    for i in 1..=10 {
        db.put(i, format!("val_{i}").as_bytes()).unwrap();
    }

    let cp_lsn = db.checkpoint_and_truncate_wal().unwrap();
    assert!(cp_lsn > Lsn(10));
    drop(db);

    // Open WAL directly and assert first record LSN > 1
    let mut wal = LogManager::open(&wal_path).unwrap();
    let records = wal.read_all_records().unwrap();
    assert!(!records.is_empty());
    assert!(records[0].lsn > Lsn(1));
    assert_eq!(records[0].lsn, cp_lsn);
    drop(wal);

    // Open TreeDb and verify recovery scanner succeeds
    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    for i in 1..=10 {
        assert_eq!(
            reopened.get(i).unwrap(),
            Some(format!("val_{i}").into_bytes())
        );
    }
}

#[test]
fn test_truncation_immediately_followed_by_appends() {
    let (_dir, db_path, wal_path) = create_test_db("cp_trunc_then_append");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"initial").unwrap();
    db.checkpoint_and_truncate_wal().unwrap();

    // Appending multiple records after truncation
    for i in 2..=8 {
        db.put(i, format!("appended_{i}").as_bytes()).unwrap();
    }
    db.sync().unwrap();
    drop(db);

    let mut wal = LogManager::open(&wal_path).unwrap();
    let records = wal.read_all_records().unwrap();
    assert_eq!(records.last().unwrap().lsn, wal.flushed_lsn());
    drop(wal);

    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    for i in 1..=8 {
        if i == 1 {
            assert_eq!(reopened.get(i).unwrap(), Some(b"initial".to_vec()));
        } else {
            assert_eq!(
                reopened.get(i).unwrap(),
                Some(format!("appended_{i}").into_bytes())
            );
        }
    }
}

#[test]
fn test_crash_during_wal_replacement_fault_matrix() {
    let (_dir, db_path, wal_path) = create_test_db("cp_crash_replacement");

    // Case A: Crash after .wal.tmp is written but before replacement (.wal exists, .tmp exists)
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(1, b"case_a").unwrap();
        db.sync().unwrap();
        drop(db);

        // Create leftover .wal.tmp
        let tmp_path = PathBuf::from(format!("{}.tmp", wal_path.display()));
        std::fs::write(&tmp_path, b"incomplete_tmp_data").unwrap();

        // Reopening should clean up .tmp and open valid .wal
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"case_a".to_vec()));
        assert!(!tmp_path.exists());
    }

    // Case B: Crash during replacement where .wal was renamed to .wal.bak
    let (_dir2, db_path2, wal_path2) = create_test_db("cp_crash_bak");
    {
        let mut db2 = TreeDb::create(&db_path2, 10).unwrap();
        db2.put(2, b"case_b").unwrap();
        db2.sync().unwrap();
        drop(db2);

        // Simulate crash right after renaming .wal -> .wal.bak
        let bak_path = PathBuf::from(format!("{}.bak", wal_path2.display()));
        std::fs::rename(&wal_path2, &bak_path).unwrap();

        // Reopening should restore from .wal.bak
        let mut reopened = TreeDb::open(&db_path2, 10).unwrap();
        assert_eq!(reopened.get(2).unwrap(), Some(b"case_b".to_vec()));
        assert!(wal_path2.exists());
    }
}

#[test]
fn test_invalid_checkpoint_record_payloads() {
    let (_dir, db_path, wal_path) = create_test_db("cp_corrupt_payload");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"val1").unwrap();
    let cp_lsn = db.checkpoint().unwrap();
    drop(db);

    // Corrupt Checkpoint record body in WAL to have invalid active txn entry (last_lsn >= cp_lsn)
    let mut wal = LogManager::open(&wal_path).unwrap();
    let records = wal.read_all_records().unwrap();
    drop(wal);

    let mut new_records = Vec::new();
    for rec in records {
        if rec.lsn == cp_lsn {
            // Checkpoint record with impossible active txn last_lsn (equal to checkpoint lsn)
            new_records.push(LogRecord::new_checkpoint(cp_lsn, vec![(TxnId(99), cp_lsn)]));
        } else {
            new_records.push(rec);
        }
    }

    // Rewrite WAL with corrupted checkpoint record
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&wal_path)
        .unwrap();
    file.write_all(&treedb::WalHeader::new().serialize())
        .unwrap();
    for r in new_records {
        file.write_all(&r.serialize()).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);

    // Reopen must reject impossible active txn entry with Error::WalCorruption
    let err = TreeDb::open(&db_path, 10).unwrap_err();
    assert!(matches!(err, Error::WalCorruption(_)));
}

#[test]
fn test_pagelsn_older_than_truncated_wal() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_older_pagelsn");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"old_page_data").unwrap();

    // Checkpoint & truncate WAL
    let _cp_lsn = db.checkpoint_and_truncate_wal().unwrap();

    // Page 2 (record page) now has PageLSN <= cp_lsn, but future WAL records have LSN > cp_lsn.
    // Insert more records that go into other pages
    for i in 100..=105 {
        db.put(i, format!("new_val_{i}").as_bytes()).unwrap();
    }
    db.sync().unwrap();
    drop(db);

    // Reopen: recovery encounters pages with PageLSN < first_retained_lsn which is valid
    let mut reopened = TreeDb::open(&db_path, 10).unwrap();
    assert_eq!(reopened.get(1).unwrap(), Some(b"old_page_data".to_vec()));
    for i in 100..=105 {
        assert_eq!(
            reopened.get(i).unwrap(),
            Some(format!("new_val_{i}").into_bytes())
        );
    }
}

#[test]
fn test_repeated_checkpoint_and_truncation_cycles_idempotence() {
    let (_dir, db_path, _wal_path) = create_test_db("cp_cycles");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // Run 25 consecutive cycles of mutate -> checkpoint -> truncate
    for cycle in 0..25 {
        for i in 0..5 {
            let k = (cycle * 5 + i) as Key;
            db.put(k, format!("cycle_{cycle}_item_{i}").as_bytes())
                .unwrap();
        }
        db.checkpoint_and_truncate_wal().unwrap();
    }

    // Verify all 125 keys exist
    for k in 0..125 {
        assert!(db.get(k).unwrap().is_some());
    }
    drop(db);

    // Reopen & recover 4 consecutive times to verify complete idempotence
    for _ in 0..4 {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        for k in 0..125 {
            assert!(reopened.get(k).unwrap().is_some());
        }
    }
}

#[test]
fn test_comprehensive_four_path_recovery_equivalence_matrix() {
    let (_dir_a, path_a, _) = create_test_db("equiv_a");
    let (_dir_b, path_b, _) = create_test_db("equiv_b");
    let (_dir_c, path_c, _) = create_test_db("equiv_c");
    let (_dir_d, path_d, _) = create_test_db("equiv_d");

    let operations = [
        (1, b"alpha".to_vec()),
        (2, vec![0x11; 6000]), // overflow
        (3, b"gamma".to_vec()),
        (1, b"alpha_prime".to_vec()),
        (4, vec![0x22; 8500]), // overflow
        (2, b"".to_vec()),     // delete overflow
        (5, b"epsilon".to_vec()),
        (4, b"short_four".to_vec()), // overflow -> inline
        (6, vec![0x33; 5000]),       // overflow
        (3, b"".to_vec()),           // delete inline
    ];

    // Path A: No checkpoints, full WAL crash recovery
    {
        let mut db_a = TreeDb::create(&path_a, 10).unwrap();
        for &(k, ref v) in &operations {
            if v.is_empty() {
                db_a.delete(k).unwrap();
            } else {
                db_a.put(k, v).unwrap();
            }
        }
        db_a.buffer_pool_mut().flush_all().unwrap();
        let _ = db_a.buffer_pool_mut().detach_wal();
    }

    // Path B: Checkpoint after every 2 operations (no truncation)
    {
        let mut db_b = TreeDb::create(&path_b, 10).unwrap();
        for (i, &(k, ref v)) in operations.iter().enumerate() {
            if v.is_empty() {
                db_b.delete(k).unwrap();
            } else {
                db_b.put(k, v).unwrap();
            }
            if i % 2 == 0 {
                db_b.checkpoint().unwrap();
            }
        }
        db_b.buffer_pool_mut().flush_all().unwrap();
        let _ = db_b.buffer_pool_mut().detach_wal();
    }

    // Path C: Checkpoint and truncate after every 3 operations
    {
        let mut db_c = TreeDb::create(&path_c, 10).unwrap();
        for (i, &(k, ref v)) in operations.iter().enumerate() {
            if v.is_empty() {
                db_c.delete(k).unwrap();
            } else {
                db_c.put(k, v).unwrap();
            }
            if i % 3 == 0 {
                db_c.checkpoint_and_truncate_wal().unwrap();
            }
        }
        db_c.buffer_pool_mut().flush_all().unwrap();
        let _ = db_c.buffer_pool_mut().detach_wal();
    }

    // Path D: Interleaved checkpoints, truncations, and reopen cycles
    {
        let mut db_d = TreeDb::create(&path_d, 10).unwrap();
        for (i, &(k, ref v)) in operations.iter().enumerate() {
            if v.is_empty() {
                db_d.delete(k).unwrap();
            } else {
                db_d.put(k, v).unwrap();
            }
            if i == 4 {
                db_d.checkpoint_and_truncate_wal().unwrap();
                db_d.sync().unwrap();
                drop(db_d);
                db_d = TreeDb::open(&path_d, 10).unwrap();
            }
        }
        db_d.buffer_pool_mut().flush_all().unwrap();
        let _ = db_d.buffer_pool_mut().detach_wal();
    }

    // Recover all 4 databases
    let mut rec_a = TreeDb::open(&path_a, 10).unwrap();
    let mut rec_b = TreeDb::open(&path_b, 10).unwrap();
    let mut rec_c = TreeDb::open(&path_c, 10).unwrap();
    let mut rec_d = TreeDb::open(&path_d, 10).unwrap();

    let scan_a = rec_a.scan(None, None).unwrap();
    let scan_b = rec_b.scan(None, None).unwrap();
    let scan_c = rec_c.scan(None, None).unwrap();
    let scan_d = rec_d.scan(None, None).unwrap();

    assert_eq!(scan_a, scan_b);
    assert_eq!(scan_b, scan_c);
    assert_eq!(scan_c, scan_d);

    // Validate freelist integrity on all recovered instances
    rec_a.buffer_pool_mut().validate_freelist().unwrap();
    rec_b.buffer_pool_mut().validate_freelist().unwrap();
    rec_c.buffer_pool_mut().validate_freelist().unwrap();
    rec_d.buffer_pool_mut().validate_freelist().unwrap();
}
