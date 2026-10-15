#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use tempfile::tempdir;
use treedb::{
    Error, LogManager, LogRecordType, Lsn, PageManager, RecoveryEngine, RecoveryFault, TreeDb,
    Value, WalReader,
};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

// ---------------------------------------------------------------------------
// 1. Recovery with many sequential WAL records
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_many_sequential_wal_records() {
    let (_dir, db_path, _wal_path) = create_test_env("many_records");

    let count = 2_000;
    {
        let mut db = TreeDb::create(&db_path, 128).unwrap();
        db.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
            .unwrap();

        for i in 0..count {
            let sql = format!("INSERT INTO items VALUES ({i}, {});", i * 10);
            db.execute_statement(&sql).unwrap();
        }
        // Drop TreeDb to simulate an un-checkpointed crash
    }

    // Reopen and recover
    let mut db = TreeDb::open(&db_path, 128).unwrap();
    for i in (0..count).step_by(50) {
        let res = db
            .execute_sql(&format!("SELECT val FROM items WHERE id = {i};"))
            .unwrap();
        assert_eq!(res.rows.len(), 1, "missing row id={i}");
        assert_eq!(res.rows[0].values[0], Value::Int64((i * 10) as i64));
    }
}

// ---------------------------------------------------------------------------
// 2. Recovery when records cross internal read-buffer boundaries
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_records_crossing_buffer_boundaries() {
    let (_dir, _db_path, wal_path) = create_test_env("buffer_crossing");

    // Write a sequence of records with various sizes to WAL
    let mut lm = LogManager::create(&wal_path).unwrap();
    let txn1 = lm.begin_txn().unwrap();

    let mut generated_lsns = Vec::new();
    for i in 0..50 {
        // Vary before/after size so record length ranges from ~45 to ~250 bytes
        let size = 16 + (i % 10) * 16;
        let before = vec![0xAA; size];
        let after = vec![0xBB; size];
        let lsn = lm
            .append_page_delta(txn1, 2 + (i as u32 % 5), (i * 8) as u16, before, after)
            .unwrap();
        generated_lsns.push(lsn);
    }
    let commit_lsn = lm.commit_txn(txn1).unwrap();
    lm.flush(commit_lsn).unwrap();
    drop(lm);

    // Read through WalReader configured with a tiny buffer: 33 bytes!
    // (Every record will span multiple read buffer refills)
    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::with_buffer_size(file, commit_lsn, 33).unwrap();
    // Seek past header
    reader.seek_to_offset(32).unwrap();

    let mut read_records = Vec::new();
    while let Some((rec, _offset, _consumed)) = reader.next_record_with_offset().unwrap() {
        read_records.push(rec);
    }

    // Total records: 1 begin + 50 deltas + 1 commit = 52 records
    assert_eq!(read_records.len(), 52);
    assert_eq!(read_records[0].record_type, LogRecordType::Begin);
    assert_eq!(read_records[51].record_type, LogRecordType::Commit);
    for (i, rec) in read_records[1..51].iter().enumerate() {
        assert_eq!(rec.record_type, LogRecordType::PageDelta);
        assert_eq!(rec.lsn, generated_lsns[i]);
    }
}

// ---------------------------------------------------------------------------
// 3. Truncated final WAL record (un-flushed crash trailing fragment)
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_truncated_final_wal_record() {
    let (_dir, db_path, wal_path) = create_test_env("truncated_final_record");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (1, 100);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (2, 200);")
            .unwrap();
        db.sync().unwrap();
    }

    // Append a partial, truncated record fragment at the very end of the WAL
    // Simulating a system crash while writing a new log record
    {
        let mut file = OpenOptions::new().append(true).open(&wal_path).unwrap();
        // A length header indicating 80 bytes of payload, but only write 12 bytes
        let mut partial = vec![80, 0, 0, 0];
        partial.extend_from_slice(&[0xFF; 12]);
        file.write_all(&partial).unwrap();
        file.sync_all().unwrap();
    }

    // Reopening database must gracefully ignore the uncommitted trailing fragment
    // and successfully recover the committed transactions
    let mut db = TreeDb::open(&db_path, 64).expect("recovery should succeed");
    let res1 = db.execute_sql("SELECT v FROM kv WHERE k = 1;").unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[0], Value::Int64(100));

    let res2 = db.execute_sql("SELECT v FROM kv WHERE k = 2;").unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::Int64(200));
}

// ---------------------------------------------------------------------------
// 4. Corruption in a record before the final record
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_rejects_corruption_before_final_record() {
    let (_dir, db_path, wal_path) = create_test_env("corrupt_non_trailing");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (1, 100);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (2, 200);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (3, 300);")
            .unwrap();
        db.sync().unwrap();
    }

    // Locate record for k=2 in WAL and corrupt its payload / CRC
    // The WAL has records for k=1, k=2, and k=3. Corrupting k=2 is strictly non-trailing.
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&wal_path)
            .unwrap();
        let file_len = file.metadata().unwrap().len();
        // Corrupt approximately at 1/2 of the file (safely before final record k=3)
        let corrupt_pos = file_len / 2;
        file.seek(SeekFrom::Start(corrupt_pos)).unwrap();
        file.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        file.sync_all().unwrap();
    }

    // Recovery MUST detect WalCorruption and fail — it must never silently ignore non-trailing corruption
    let res = TreeDb::open(&db_path, 64);
    assert!(
        res.is_err(),
        "recovery must reject non-trailing record corruption"
    );
    match res.err().unwrap() {
        Error::WalCorruption(msg) => {
            assert!(
                msg.contains("WAL corrupted")
                    || msg.contains("Durable WAL corrupted")
                    || msg.contains("CRC mismatch")
                    || msg.contains("invalid"),
                "unexpected corruption error message: {msg}"
            );
        }
        other => panic!("expected Error::WalCorruption, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. Recovery after insert, update, and delete operations
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_insert_update_delete_lifecycle() {
    let (_dir, db_path, _wal_path) = create_test_env("dml_lifecycle");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT);")
            .unwrap();

        // 1. Inserts
        db.execute_statement("INSERT INTO accounts VALUES (1, 1000);")
            .unwrap();
        db.execute_statement("INSERT INTO accounts VALUES (2, 2000);")
            .unwrap();
        db.execute_statement("INSERT INTO accounts VALUES (3, 3000);")
            .unwrap();

        // 2. Updates
        db.execute_statement("UPDATE accounts SET balance = 1500 WHERE id = 1;")
            .unwrap();
        db.execute_statement("UPDATE accounts SET balance = 2500 WHERE id = 2;")
            .unwrap();

        // 3. Delete
        db.execute_statement("DELETE FROM accounts WHERE id = 3;")
            .unwrap();

        // Flush WAL
        db.sync().unwrap();
    }

    // Reopen and verify final state
    let mut db = TreeDb::open(&db_path, 64).unwrap();

    let r1 = db
        .execute_sql("SELECT balance FROM accounts WHERE id = 1;")
        .unwrap();
    assert_eq!(r1.rows[0].values[0], Value::Int64(1500));

    let r2 = db
        .execute_sql("SELECT balance FROM accounts WHERE id = 2;")
        .unwrap();
    assert_eq!(r2.rows[0].values[0], Value::Int64(2500));

    let r3 = db
        .execute_sql("SELECT balance FROM accounts WHERE id = 3;")
        .unwrap();
    assert_eq!(
        r3.rows.len(),
        0,
        "deleted record must not exist after recovery"
    );
}

// ---------------------------------------------------------------------------
// 6. Recovery with transaction commit and abort records
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_with_commit_and_abort_and_loser_txns() {
    let (_dir, db_path, wal_path) = create_test_env("commit_abort_loser");

    // Manually construct WAL records to accurately simulate crash with:
    // T1: committed
    // T2: aborted (with abort log record)
    // T3: active loser transaction (crashed before commit/abort)
    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 performs write and commits
    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 8], b"T1_COMMT".to_vec())
        .unwrap();
    let c1 = lm.commit_txn(t1).unwrap();

    // T2 performs write and aborts
    let t2 = lm.begin_txn().unwrap();
    lm.append_page_delta(t2, p1, 80, vec![0; 8], b"T2_ABORT".to_vec())
        .unwrap();
    lm.abort_txn(t2).unwrap();

    // T3 performs write and crashes without commit or abort
    let t3 = lm.begin_txn().unwrap();
    lm.append_page_delta(t3, p1, 96, vec![0; 8], b"T3_LOSER".to_vec())
        .unwrap();

    lm.flush(c1).unwrap();
    drop(pm);
    drop(lm);

    // Run recovery
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut lm_rec = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm_rec, &mut lm_rec).unwrap();

    // Verify recovery statistics:
    // T1 was redone. T3 was identified as a loser and undone.
    assert!(stats.redo_count >= 1);
    assert_eq!(stats.undo_count, 1, "T3 should be undone");
    assert_eq!(stats.loser_txns, 1, "T3 was the single active loser");
    assert_eq!(
        stats.active_txns, 3,
        "All 3 transactions tracked in analysis pass"
    );

    // Verify disk page state:
    // T1_COMMT must be present at offset 64
    // T3_LOSER must be undone back to zeros at offset 96
    let page = pm_rec.read_page(p1).unwrap();
    let bytes = page.as_bytes();
    assert_eq!(&bytes[64..72], b"T1_COMMT");
    assert_eq!(&bytes[96..104], &[0; 8], "T3 loser write must be undone");
}

// ---------------------------------------------------------------------------
// 7. Recovery following checkpoint creation
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_following_checkpoint() {
    let (_dir, db_path, _wal_path) = create_test_env("checkpoint_recovery");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();

        for i in 1..=50 {
            db.execute_statement(&format!("INSERT INTO kv VALUES ({i}, {});", i * 10))
                .unwrap();
        }

        // Take a checkpoint
        db.checkpoint().unwrap();

        // More operations after checkpoint
        for i in 51..=100 {
            db.execute_statement(&format!("INSERT INTO kv VALUES ({i}, {});", i * 10))
                .unwrap();
        }
        db.sync().unwrap();
    }

    // Recover
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    for i in 1..=100 {
        let res = db
            .execute_sql(&format!("SELECT v FROM kv WHERE k = {i};"))
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].values[0], Value::Int64((i * 10) as i64));
    }
}

// ---------------------------------------------------------------------------
// 8. Recovery with a WAL that has not been truncated
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_with_untruncated_wal_spanning_checkpoint() {
    let (_dir, db_path, wal_path) = create_test_env("untruncated_wal");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // 1. Transaction 1 commits and dirty page is flushed at checkpoint
    let t1 = lm.begin_txn().unwrap();
    let lsn1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], b"ABCD".to_vec())
        .unwrap();
    let _c1 = lm.commit_txn(t1).unwrap();

    let mut page = pm.read_page(p1).unwrap();
    page.as_bytes_mut()[64..68].copy_from_slice(b"ABCD");
    page.set_lsn(lsn1);
    pm.write_page(&page).unwrap();

    // 2. Active Transaction 2 begins
    let t2 = lm.begin_txn().unwrap();
    let lsn2 = lm
        .append_page_delta(t2, p1, 80, vec![0; 4], b"WXYZ".to_vec())
        .unwrap();

    // 3. Checkpoint written while T2 is active (do NOT truncate WAL)
    let cp_lsn = lm.append_checkpoint(vec![(t2, lsn2)]).unwrap();
    pm.set_last_checkpoint_lsn(cp_lsn).unwrap();

    // 4. Transaction 2 commits after checkpoint
    let c2 = lm.commit_txn(t2).unwrap();
    lm.flush(c2).unwrap();

    drop(pm);
    drop(lm);

    // Recover with untruncated WAL
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut lm_rec = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm_rec, &mut lm_rec).unwrap();

    assert_eq!(stats.checkpoint_lsn, cp_lsn);
    assert_eq!(stats.undo_count, 0);

    let page = pm_rec.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], b"ABCD");
    assert_eq!(&page.as_bytes()[80..84], b"WXYZ");
}

// ---------------------------------------------------------------------------
// 9. Repeated recovery/reopen idempotence
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_repeated_reopen_idempotence() {
    let (_dir, db_path, _wal_path) = create_test_env("repeated_reopen");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE t (id INT PRIMARY KEY, name TEXT);")
            .unwrap();
        db.execute_statement("INSERT INTO t VALUES (1, 'Alice');")
            .unwrap();
        db.execute_statement("INSERT INTO t VALUES (2, 'Bob');")
            .unwrap();
        db.sync().unwrap();
    }

    // First recovery
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let res = db.execute_sql("SELECT name FROM t ORDER BY id;").unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
        assert_eq!(res.rows[1].values[0], Value::String("Bob".to_string()));
    }

    // Second recovery without changes
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let res = db.execute_sql("SELECT name FROM t ORDER BY id;").unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
        assert_eq!(res.rows[1].values[0], Value::String("Bob".to_string()));
    }

    // Third recovery without changes
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let res = db.execute_sql("SELECT name FROM t ORDER BY id;").unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
        assert_eq!(res.rows[1].values[0], Value::String("Bob".to_string()));
    }
}

// ---------------------------------------------------------------------------
// 10. Differential verification against reference model
// ---------------------------------------------------------------------------
#[test]
fn test_differential_verification_against_oracle() {
    let (_dir, db_path, _wal_path) = create_test_env("differential_oracle");

    let mut oracle: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1000, b"seed_val").unwrap();
        oracle.insert(1000, b"seed_val".to_vec());

        // Apply a deterministic mix of inserts, updates, deletes, commits, and aborts
        for step in 0..300 {
            let key = 1 + (step * 37) % 50;
            let val = format!("val_{step}_{key}").into_bytes();

            if step % 7 == 0 {
                // Aborted transaction
                let mut txn = db.begin().unwrap();
                txn.put(key, &val).unwrap();
                txn.abort().unwrap();
                // Oracle does not reflect aborted write
            } else if step % 5 == 0 {
                // Delete
                let mut txn = db.begin().unwrap();
                txn.delete(key).unwrap();
                txn.commit().unwrap();
                oracle.remove(&key);
            } else {
                // Committed put (insert or update)
                let mut txn = db.begin().unwrap();
                txn.put(key, &val).unwrap();
                txn.commit().unwrap();
                oracle.insert(key, val);
            }
        }
        db.sync().unwrap();
    }

    // Crash and recover
    let mut db = TreeDb::open(&db_path, 64).unwrap();

    // Verify all keys from 1 to 50 and seed key against oracle
    for key in 1..=50 {
        let actual = db.get(key).unwrap();
        let expected = oracle.get(&key).cloned();
        assert_eq!(
            actual, expected,
            "differential mismatch for key {key}: actual={actual:?}, expected={expected:?}"
        );
    }
    assert_eq!(db.get(1000).unwrap(), Some(b"seed_val".to_vec()));
}

// ---------------------------------------------------------------------------
// 11. Bounded reader-buffer behavior assertions
// ---------------------------------------------------------------------------
#[test]
fn test_bounded_reader_buffer_behavior() {
    let (_dir, _db_path, wal_path) = create_test_env("bounded_buffer");

    // Write 500 records into the WAL (~50 KB)
    let mut lm = LogManager::create(&wal_path).unwrap();
    for _ in 0..500 {
        let txn = lm.begin_txn().unwrap();
        lm.append_page_delta(txn, 1, 64, vec![0; 32], vec![1; 32])
            .unwrap();
        lm.commit_txn(txn).unwrap();
    }
    let flushed = lm.flushed_lsn();
    drop(lm);

    // Read with a strict 4 KB buffer
    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::with_buffer_size(file, flushed, 4 * 1024).unwrap();
    reader.seek_to_offset(32).unwrap();

    let mut record_count = 0;
    while let Some((_rec, _offset, _consumed)) = reader.next_record_with_offset().unwrap() {
        record_count += 1;
    }

    // 500 txns * (1 begin + 1 delta + 1 commit) = 1500 records
    assert_eq!(record_count, 1500);
}

// ---------------------------------------------------------------------------
// 12. Failure-injection during recovery
// ---------------------------------------------------------------------------
#[test]
fn test_recovery_under_injected_faults() {
    let (_dir, db_path, wal_path) = create_test_env("fault_injection");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], b"TEST".to_vec())
        .unwrap();
    // Leave t1 uncommitted so undo pass has work to do
    lm.flush(Lsn(2)).unwrap();
    drop(pm);
    drop(lm);

    // Fault 1: BeforeRedo (crashes right after analysis)
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let err =
            RecoveryEngine::recover_with_fault(&mut pm, &mut lm, Some(RecoveryFault::BeforeRedo))
                .unwrap_err();
        assert!(matches!(err, Error::RecoveryInterrupted(_)));
    }

    // Fault 2: BeforeUndo (crashes right after redo)
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let err =
            RecoveryEngine::recover_with_fault(&mut pm, &mut lm, Some(RecoveryFault::BeforeUndo))
                .unwrap_err();
        assert!(matches!(err, Error::RecoveryInterrupted(_)));
    }

    // Clean recovery run to completion
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.undo_count, 1);
        assert_eq!(stats.active_txns, 1);
        assert_eq!(stats.loser_txns, 1);
    }
}
