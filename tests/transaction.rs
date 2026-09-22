use std::collections::BTreeMap;
use std::path::PathBuf;
use tempfile::TempDir;

use treedb::{Key, LogManager, LogRecordType, TreeDb, TxnState};

fn create_test_db(name: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join(format!("{name}.db"));
    let wal_path = dir.path().join(format!("{name}.db.wal"));
    (dir, db_path, wal_path)
}

// ---------------------------------------------------------------------------
// 1. LIFECYCLE & STATE MACHINE TESTS
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_lifecycle_basic() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_lifecycle_basic");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    let mut txn = db.begin().unwrap();
    assert_eq!(txn.state(), TxnState::Active);
    assert!(txn.is_active());

    txn.put(1, b"val1").unwrap();
    txn.put(2, b"val2").unwrap();

    txn.commit().unwrap();

    // Re-accessing database after commit reflects committed data
    assert_eq!(db.get(1).unwrap(), Some(b"val1".to_vec()));
    assert_eq!(db.get(2).unwrap(), Some(b"val2".to_vec()));
}

#[test]
fn test_transaction_already_active_error() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_already_active");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    let txn1 = db.begin().unwrap();
    let txn1_id = txn1.id();

    // Verify transaction 1 is active
    assert!(txn1.is_active());
    assert_eq!(txn1.id(), txn1_id);
    txn1.commit().unwrap();

    // Now a new transaction can begin cleanly
    let txn2 = db.begin().unwrap();
    assert!(txn2.is_active());
    txn2.commit().unwrap();
}

#[test]
fn test_transaction_abort_lifecycle() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_abort_lifecycle");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"initial1").unwrap();
    db.put(2, b"initial2").unwrap();

    let mut txn = db.begin().unwrap();
    txn.put(1, b"modified1").unwrap();
    txn.put(3, b"new3").unwrap();
    txn.delete(2).unwrap();

    txn.abort().unwrap();

    // Database should be in the exact initial state
    assert_eq!(db.get(1).unwrap(), Some(b"initial1".to_vec()));
    assert_eq!(db.get(2).unwrap(), Some(b"initial2".to_vec()));
    assert_eq!(db.get(3).unwrap(), None);
}

// ---------------------------------------------------------------------------
// 2. ATOMICITY & MULTI-OPERATION SEQUENCES
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_multi_operation_atomicity() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_multi_atomicity");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    for i in 1..=5 {
        db.put(i, format!("init_{i}").as_bytes()).unwrap();
    }

    // Atomic transaction modifying multiple keys
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, b"updated_1").unwrap();
        txn.delete(2).unwrap();
        txn.put(6, b"inserted_6").unwrap();
        txn.update(3, b"updated_3").unwrap();
        txn.delete(4).unwrap();
        txn.put(7, b"inserted_7").unwrap();
        txn.commit().unwrap();
    }

    assert_eq!(db.get(1).unwrap(), Some(b"updated_1".to_vec()));
    assert_eq!(db.get(2).unwrap(), None);
    assert_eq!(db.get(3).unwrap(), Some(b"updated_3".to_vec()));
    assert_eq!(db.get(4).unwrap(), None);
    assert_eq!(db.get(5).unwrap(), Some(b"init_5".to_vec()));
    assert_eq!(db.get(6).unwrap(), Some(b"inserted_6".to_vec()));
    assert_eq!(db.get(7).unwrap(), Some(b"inserted_7".to_vec()));
}

#[test]
fn test_transaction_same_key_repeated_mutations() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_same_key_repeated");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // Case 1: put(1, A) -> put(1, B) -> put(1, C) -> commit
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, b"A").unwrap();
        txn.put(1, b"B").unwrap();
        txn.put(1, b"C").unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(db.get(1).unwrap(), Some(b"C".to_vec()));

    // Case 2: put(1, A) -> delete(1) -> commit
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, b"A").unwrap();
        txn.delete(1).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(db.get(1).unwrap(), None);

    // Case 3: put(1, A) -> delete(1) -> put(1, B) -> commit
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, b"A").unwrap();
        txn.delete(1).unwrap();
        txn.put(1, b"B").unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(db.get(1).unwrap(), Some(b"B".to_vec()));

    // Case 4: delete(1) -> put(1, X) -> abort
    {
        let mut txn = db.begin().unwrap();
        txn.delete(1).unwrap();
        txn.put(1, b"X").unwrap();
        txn.abort().unwrap();
    }
    assert_eq!(db.get(1).unwrap(), Some(b"B".to_vec()));
}

// ---------------------------------------------------------------------------
// 3. READ-YOUR-WRITES & SCAN VISIBILITY
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_read_your_writes() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_ryw");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(10, b"old_10").unwrap();

    let mut txn = db.begin().unwrap();

    // Read initial data
    assert_eq!(txn.get(10).unwrap(), Some(b"old_10".to_vec()));
    assert_eq!(txn.get(20).unwrap(), None);

    // Insert key 20 and read back
    txn.put(20, b"new_20").unwrap();
    assert_eq!(txn.get(20).unwrap(), Some(b"new_20".to_vec()));

    // Update key 10 and read back
    txn.put(10, b"updated_10").unwrap();
    assert_eq!(txn.get(10).unwrap(), Some(b"updated_10".to_vec()));

    // Delete key 10 and read back
    txn.delete(10).unwrap();
    assert_eq!(txn.get(10).unwrap(), None);

    txn.commit().unwrap();
}

#[test]
fn test_transaction_scan_visibility() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_scan_vis");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"v1").unwrap();
    db.put(3, b"v3").unwrap();
    db.put(5, b"v5").unwrap();

    let mut txn = db.begin().unwrap();

    // In-transaction modifications
    txn.put(2, b"v2").unwrap();
    txn.delete(3).unwrap();
    txn.put(4, b"v4").unwrap();

    // Scan should reflect in-transaction state: [1, 2, 4, 5]
    let scanned = txn.scan(None, None).unwrap();
    let keys: Vec<Key> = scanned.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, vec![1, 2, 4, 5]);

    txn.abort().unwrap();

    // Post-abort scan reflects original state: [1, 3, 5]
    let post_scan = db.scan(None, None).unwrap();
    let post_keys: Vec<Key> = post_scan.iter().map(|(k, _)| *k).collect();
    assert_eq!(post_keys, vec![1, 3, 5]);
}

// ---------------------------------------------------------------------------
// 4. TRANSACTION DROP BEHAVIOR (AUTO-ROLLBACK)
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_drop_auto_rollback() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_drop_rollback");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    db.put(1, b"anchor1").unwrap();

    // Scope begins and drops transaction without commit
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, b"modified1").unwrap();
        txn.put(2, b"uncommitted2").unwrap();
        txn.delete(1).unwrap();
        // txn dropped here without commit
    }

    // Database should be automatically rolled back cleanly
    assert_eq!(db.get(1).unwrap(), Some(b"anchor1".to_vec()));
    assert_eq!(db.get(2).unwrap(), None);

    // Can start a new transaction immediately without errors
    let mut txn2 = db.begin().unwrap();
    txn2.put(3, b"committed3").unwrap();
    txn2.commit().unwrap();

    assert_eq!(db.get(3).unwrap(), Some(b"committed3".to_vec()));
}

// ---------------------------------------------------------------------------
// 5. PERSISTENCE & CRASH RECOVERY
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_committed_persistence_across_reopen() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_reopen_persist");

    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        let mut txn = db.begin().unwrap();
        for i in 1..=50 {
            txn.put(i, format!("val_{i}").as_bytes()).unwrap();
        }
        txn.commit().unwrap();
        db.sync().unwrap();
    }

    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        for i in 1..=50 {
            assert_eq!(
                reopened.get(i).unwrap(),
                Some(format!("val_{i}").into_bytes())
            );
        }
    }
}

#[test]
fn test_transaction_crash_before_commit_recovery() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_crash_before_commit");

    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(1, b"durable_1").unwrap();

        let mut txn = db.begin().unwrap();
        txn.put(2, b"uncommitted_2").unwrap();
        txn.put(3, b"uncommitted_3").unwrap();
        txn.put(1, b"corrupted_1").unwrap();

        // Simulate sudden crash: flush dirty pages to disk, prevent Drop cleanup
        txn.buffer_pool_mut().flush_all().unwrap();
        let _ = txn.buffer_pool_mut().detach_wal();
        std::mem::forget(txn);
    }

    // Reopen and recover
    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"durable_1".to_vec()));
        assert_eq!(reopened.get(2).unwrap(), None);
        assert_eq!(reopened.get(3).unwrap(), None);
    }
}

#[test]
fn test_transaction_crash_after_commit_recovery() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_crash_after_commit");

    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        let mut txn = db.begin().unwrap();
        txn.put(10, b"commit_10").unwrap();
        txn.put(20, b"commit_20").unwrap();
        txn.commit().unwrap();

        // Simulate crash right after commit (pages not synced)
        let _ = db.buffer_pool_mut().detach_wal();
    }

    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(10).unwrap(), Some(b"commit_10".to_vec()));
        assert_eq!(reopened.get(20).unwrap(), Some(b"commit_20".to_vec()));
    }
}

// ---------------------------------------------------------------------------
// 6. CHECKPOINT & WAL TRUNCATION INTERACTION
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_spanning_checkpoints_commit_and_abort() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_spanning_checkpoints");

    // Case A: Active transaction spans checkpoints and commits
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(1, b"anchor").unwrap();

        let mut txn = db.begin().unwrap();
        txn.put(10, b"t_v1").unwrap();

        // Checkpoint 1 during active transaction
        txn.checkpoint().unwrap();

        txn.put(11, b"t_v2").unwrap();

        // Checkpoint 2 during active transaction
        txn.checkpoint().unwrap();

        txn.put(12, b"t_v3").unwrap();
        txn.commit().unwrap();
        db.sync().unwrap();
    }

    {
        let mut reopened = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"anchor".to_vec()));
        assert_eq!(reopened.get(10).unwrap(), Some(b"t_v1".to_vec()));
        assert_eq!(reopened.get(11).unwrap(), Some(b"t_v2".to_vec()));
        assert_eq!(reopened.get(12).unwrap(), Some(b"t_v3".to_vec()));
    }

    // Case B: Active transaction spans checkpoints and truncations, then crashes uncommitted
    let (_dir2, db_path2, wal_path2) = create_test_db("txn_spanning_trunc_crash");
    {
        let mut db = TreeDb::create(&db_path2, 10).unwrap();
        db.put(1, b"anchor").unwrap();

        let mut txn = db.begin().unwrap();
        let t_id = txn.id();
        txn.put(100, b"uncommitted_100").unwrap();

        // Checkpoint and truncate WAL while transaction is active
        let cp_lsn = txn.checkpoint_and_truncate_wal().unwrap();

        // Verify WAL retains active transaction's earliest LSN
        let mut wal = LogManager::open(&wal_path2).unwrap();
        let records = wal.read_all_records().unwrap();
        assert!(records[0].lsn <= cp_lsn);
        assert!(records
            .iter()
            .any(|r| r.txn_id == t_id && r.record_type == LogRecordType::Begin));
        drop(wal);

        txn.put(101, b"uncommitted_101").unwrap();

        // Simulate crash
        txn.buffer_pool_mut().flush_all().unwrap();
        let _ = txn.buffer_pool_mut().detach_wal();
        std::mem::forget(txn);
    }

    {
        let mut reopened = TreeDb::open(&db_path2, 10).unwrap();
        assert_eq!(reopened.get(1).unwrap(), Some(b"anchor".to_vec()));
        assert_eq!(reopened.get(100).unwrap(), None);
        assert_eq!(reopened.get(101).unwrap(), None);
    }
}

// ---------------------------------------------------------------------------
// 7. PAGE REUSE & FREELIST INTEGRITY
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_freelist_recycling_and_rollback() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_freelist_rollback");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    // Populate enough keys to trigger leaf node allocations
    for i in 1..=300 {
        db.put(i, format!("val_{i}").as_bytes()).unwrap();
    }
    db.sync().unwrap();
    let initial_free_count = db.buffer_pool().page_manager().header().free_page_count;

    // Transaction that deletes many keys (reclaiming pages into freelist) and aborts
    {
        let mut txn = db.begin().unwrap();
        for i in 150..=300 {
            txn.delete(i).unwrap();
        }
        txn.abort().unwrap();
    }

    // Freelist and all keys should be restored to pre-transaction state
    assert_eq!(
        db.buffer_pool().page_manager().header().free_page_count,
        initial_free_count
    );
    for i in 1..=300 {
        assert_eq!(db.get(i).unwrap(), Some(format!("val_{i}").into_bytes()));
    }
    db.buffer_pool_mut().validate_freelist().unwrap();
    db.validate_tree().unwrap();
}

// ---------------------------------------------------------------------------
// 8. MULTI-PAGE OVERFLOW TRANSACTIONS
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_overflow_operations_and_rollback() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_overflow_rollback");

    let mut db = TreeDb::create(&db_path, 10).unwrap();

    let small = b"small_val".to_vec();
    let huge1 = vec![0x41; 9000]; // Multi-page overflow (~3 pages)
    let huge2 = vec![0x42; 15000]; // Larger multi-page overflow (~4 pages)

    // Transaction committing mixed inline and overflow mutations
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, &small).unwrap();
        txn.put(2, &huge1).unwrap();
        txn.commit().unwrap();
    }

    assert_eq!(db.get(1).unwrap(), Some(small.clone()));
    assert_eq!(db.get(2).unwrap(), Some(huge1.clone()));

    // Transaction performing transitions (inline -> overflow, overflow -> larger, delete) and aborting
    {
        let mut txn = db.begin().unwrap();
        txn.put(1, &huge2).unwrap(); // inline -> overflow
        txn.put(2, &small).unwrap(); // overflow -> inline
        txn.put(3, &huge1).unwrap(); // new overflow
        txn.delete(2).unwrap(); // delete
        txn.abort().unwrap();
    }

    // Verify exact pre-transaction contents
    assert_eq!(db.get(1).unwrap(), Some(small));
    assert_eq!(db.get(2).unwrap(), Some(huge1));
    assert_eq!(db.get(3).unwrap(), None);

    db.buffer_pool_mut().validate_freelist().unwrap();
}

// ---------------------------------------------------------------------------
// 9. RANDOMIZED ORACLE COMPARISON
// ---------------------------------------------------------------------------

#[test]
fn test_transaction_randomized_oracle_comparison() {
    let (_dir, db_path, _wal_path) = create_test_db("txn_randomized_oracle");

    let mut db = TreeDb::create(&db_path, 10).unwrap();
    let mut oracle: BTreeMap<Key, Vec<u8>> = BTreeMap::new();

    // Deterministic pseudo-random sequence generator
    let mut lcg = 0x12345678u64;
    let mut next_rand = || -> u64 {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
        lcg
    };

    for txn_round in 0..50 {
        let should_commit = (next_rand() % 10) < 7; // 70% commit, 30% abort
        let num_ops = (next_rand() % 8 + 1) as usize; // 1..=8 operations per txn

        let mut local_oracle = oracle.clone();
        let mut txn = db.begin().unwrap();

        for _ in 0..num_ops {
            let op_type = next_rand() % 4;
            let key = (next_rand() % 40) as Key;

            match op_type {
                0 => {
                    // Put small/medium value
                    let val_len = (next_rand() % 100 + 1) as usize;
                    let val = vec![(key % 256) as u8; val_len];
                    txn.put(key, &val).unwrap();
                    local_oracle.insert(key, val);
                }
                1 => {
                    // Put overflow value
                    let val_len = (next_rand() % 5000 + 4100) as usize;
                    let val = vec![(key % 256) as u8; val_len];
                    txn.put(key, &val).unwrap();
                    local_oracle.insert(key, val);
                }
                2 => {
                    // Delete
                    let deleted = txn.delete(key).unwrap();
                    let oracle_deleted = local_oracle.remove(&key);
                    assert_eq!(deleted, oracle_deleted);
                }
                _ => {
                    // Get (Read-Your-Writes validation)
                    let val = txn.get(key).unwrap();
                    let oracle_val = local_oracle.get(&key).cloned();
                    assert_eq!(val, oracle_val);
                }
            }
        }

        if should_commit {
            txn.commit().unwrap();
            oracle = local_oracle;
        } else {
            txn.abort().unwrap();
        }

        // Periodically checkpoint & validate
        if txn_round % 10 == 0 {
            db.checkpoint_and_truncate_wal().unwrap();
            db.buffer_pool_mut().validate_freelist().unwrap();
            db.validate_tree().unwrap();
        }
    }

    // Final comparison against oracle
    for key in 0..40 {
        assert_eq!(db.get(key).unwrap(), oracle.get(&key).cloned());
    }

    let all_scanned = db.scan(None, None).unwrap();
    let oracle_entries: Vec<(Key, Vec<u8>)> = oracle.into_iter().collect();
    assert_eq!(all_scanned, oracle_entries);

    db.buffer_pool_mut().validate_freelist().unwrap();
    db.validate_tree().unwrap();
}
