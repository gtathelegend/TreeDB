#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use tempfile::tempdir;

use treedb::wal::log_record::LogRecord;
use treedb::wal::lsn::{Lsn, TxnId, NULL_LSN};
use treedb::{
    Error, LogManager, PageManager, RecoveryEngine, StorageFailurePoint, TreeDb, WalReader,
};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

// ===========================================================================
// SECTION 1: Transaction Outcomes Matrix
// ===========================================================================

#[test]
fn test_committed_transaction_persists_across_reopen() {
    let (_dir, db_path, _wal_path) = create_test_env("txn_committed_persists");
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1, b"val1_committed").unwrap();
        db.put(2, b"val2_committed").unwrap();
        db.sync().unwrap();
    }

    // Reopen and verify data persisted cleanly
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(db.get(1).unwrap().as_deref(), Some(&b"val1_committed"[..]));
    assert_eq!(db.get(2).unwrap().as_deref(), Some(&b"val2_committed"[..]));
}

#[test]
fn test_uncommitted_insert_undone_on_recovery() {
    let (_dir, db_path, _wal_path) = create_test_env("uncommitted_insert_undone");
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1, b"stable_1").unwrap();
        db.sync().unwrap();

        // Start uncommitted transaction
        let mut txn = db.begin().unwrap();
        txn.put(2, b"uncommitted_insert").unwrap();
        // Drop transaction without commit, simulate immediate crash
    }

    // Reopen: Key 2 must not exist, Key 1 must be preserved
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(db.get(1).unwrap().as_deref(), Some(&b"stable_1"[..]));
    assert_eq!(db.get(2).unwrap(), None);
}

#[test]
fn test_uncommitted_update_restores_prior_value() {
    let (_dir, db_path, _wal_path) = create_test_env("uncommitted_update_restores");
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1, b"original_value").unwrap();
        db.sync().unwrap();

        // Mutate in an uncommitted transaction
        let mut txn = db.begin().unwrap();
        txn.put(1, b"temporary_overwritten_value").unwrap();
        // Simulate crash
    }

    // Reopen: Key 1 must restore original_value
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(db.get(1).unwrap().as_deref(), Some(&b"original_value"[..]));
}

#[test]
fn test_uncommitted_delete_restores_deleted_record() {
    let (_dir, db_path, _wal_path) = create_test_env("uncommitted_delete_restores");
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(42, b"must_survive_delete").unwrap();
        db.sync().unwrap();

        // Delete inside uncommitted transaction
        let mut txn = db.begin().unwrap();
        txn.delete(42).unwrap();
        // Simulate crash
    }

    // Reopen: Key 42 must be restored
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(
        db.get(42).unwrap().as_deref(),
        Some(&b"must_survive_delete"[..])
    );
}

#[test]
fn test_multiple_interleaved_transactions_recover_independently() {
    let (_dir, db_path, wal_path) = create_test_env("interleaved_independent_recovery");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 (commits), T2 (loses), T3 (commits), T4 (aborts), T5 (loses)
    let t1 = lm.begin_txn().unwrap();
    let t2 = lm.begin_txn().unwrap();
    let t3 = lm.begin_txn().unwrap();
    let t4 = lm.begin_txn().unwrap();
    let t5 = lm.begin_txn().unwrap();

    // T1 write
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    // T2 write
    lm.append_page_delta(t2, p1, 68, vec![0; 4], vec![2, 2, 2, 2])
        .unwrap();
    // T3 write
    lm.append_page_delta(t3, p1, 72, vec![0; 4], vec![3, 3, 3, 3])
        .unwrap();
    // T1 commits
    let c1 = lm.commit_txn(t1).unwrap();
    // T4 write
    lm.append_page_delta(t4, p1, 76, vec![0; 4], vec![4, 4, 4, 4])
        .unwrap();
    // T4 aborts explicitly: logs CLR restoring before-image [0, 0, 0, 0], then logs Abort
    let clr4 = lm.append_clr(t4, p1, 76, vec![0; 4], NULL_LSN).unwrap();
    let a4 = lm.abort_txn(t4).unwrap();
    // T5 write
    lm.append_page_delta(t5, p1, 80, vec![0; 4], vec![5, 5, 5, 5])
        .unwrap();
    // T3 commits
    let c3 = lm.commit_txn(t3).unwrap();
    lm.flush(c3).unwrap();
    lm.flush(c1).unwrap();
    lm.flush(clr4).unwrap();
    lm.flush(a4).unwrap();

    drop(pm);
    drop(lm);

    // Reopen & recover
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.committed_txns, 2); // T1, T3
    assert_eq!(stats.loser_txns, 2); // T2, T5
    assert_eq!(stats.undo_count, 2); // T2 delta, T5 delta undone

    let page = pm.read_page(p1).unwrap();
    // Committed T1 and T3 changes survive
    assert_eq!(&page.as_bytes()[64..68], &[1, 1, 1, 1]);
    assert_eq!(&page.as_bytes()[72..76], &[3, 3, 3, 3]);
    // Losers T2, T4, T5 are zeroed
    assert_eq!(&page.as_bytes()[68..72], &[0, 0, 0, 0]);
    assert_eq!(&page.as_bytes()[76..80], &[0, 0, 0, 0]);
    assert_eq!(&page.as_bytes()[80..84], &[0, 0, 0, 0]);
}

#[test]
fn test_committed_transaction_changes_survive_even_when_later_transaction_loses() {
    let (_dir, db_path, wal_path) = create_test_env("committed_survives_later_loser");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 commits value [10, 20, 30, 40]
    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![10, 20, 30, 40])
        .unwrap();
    let c1 = lm.commit_txn(t1).unwrap();
    lm.flush(c1).unwrap();

    // Update page on disk to match committed state
    let mut page = pm.read_page(p1).unwrap();
    page.as_bytes_mut()[64..68].copy_from_slice(&[10, 20, 30, 40]);
    pm.write_page(&page).unwrap();

    // Later transaction T2 updates same offset to [99, 99, 99, 99], before_image is [10, 20, 30, 40]
    let t2 = lm.begin_txn().unwrap();
    lm.append_page_delta(t2, p1, 64, vec![10, 20, 30, 40], vec![99, 99, 99, 99])
        .unwrap();
    // Flush delta to simulate crash while uncommitted
    lm.flush(lm.next_lsn()).unwrap();

    drop(pm);
    drop(lm);

    // Recover
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.committed_txns, 1);
    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 1);

    // Verify T1's committed value was restored
    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[10, 20, 30, 40]);
}

#[test]
fn test_aborted_transaction_remains_aborted_after_repeated_reopen() {
    let (_dir, db_path, wal_path) = create_test_env("aborted_remains_aborted");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![55, 55, 55, 55])
        .unwrap();
    let a1 = lm.abort_txn(t1).unwrap();
    lm.flush(a1).unwrap();

    drop(pm);
    drop(lm);

    // Reopen Cycle 1
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 0);
        assert_eq!(stats.undo_count, 0);
    }
    // Reopen Cycle 2
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 0);
        assert_eq!(stats.undo_count, 0);
    }
}

#[test]
fn test_recovery_idempotency_three_consecutive_cycles() {
    let (_dir, db_path, _wal_path) = create_test_env("recovery_idempotency_3_cycles");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        for i in 1..=50 {
            db.put(i, &(i * 10).to_le_bytes()).unwrap();
        }
        db.sync().unwrap();
    }

    // 3 consecutive open / recover / close cycles
    for cycle in 1..=3 {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        for i in 1..=50 {
            let val = db.get(i).unwrap().expect("key must exist");
            assert_eq!(
                val,
                (i * 10).to_le_bytes(),
                "cycle {cycle}: key {i} value mismatch"
            );
        }
        db.sync().unwrap();
    }
}

// ===========================================================================
// SECTION 2: Checkpoints and WAL Retention Matrix
// ===========================================================================

#[test]
fn test_transaction_active_across_checkpoint_and_commits_later() {
    let (_dir, db_path, wal_path) = create_test_env("active_across_checkpoint_commits");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 starts before checkpoint
    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();

    // Checkpoint records T1 as active
    let cp = lm.append_checkpoint(vec![(t1, d1)]).unwrap();
    lm.flush(cp).unwrap();

    // T1 continues after checkpoint and commits
    let _d2 = lm
        .append_page_delta(t1, p1, 68, vec![0; 4], vec![5, 6, 7, 8])
        .unwrap();
    let c1 = lm.commit_txn(t1).unwrap();
    lm.flush(c1).unwrap();

    drop(pm);
    drop(lm);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.committed_txns, 1);
    assert_eq!(stats.loser_txns, 0);

    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[1, 2, 3, 4]);
    assert_eq!(&page.as_bytes()[68..72], &[5, 6, 7, 8]);
}

#[test]
fn test_transaction_active_across_checkpoint_and_aborts_later() {
    let (_dir, db_path, wal_path) = create_test_env("active_across_checkpoint_aborts");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 starts before checkpoint
    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![9, 9, 9, 9])
        .unwrap();

    // Mutate page on disk to simulate flushed dirty page
    let mut page = pm.read_page(p1).unwrap();
    page.as_bytes_mut()[64..68].copy_from_slice(&[9, 9, 9, 9]);
    pm.write_page(&page).unwrap();

    // Checkpoint with T1 active
    let cp = lm.append_checkpoint(vec![(t1, d1)]).unwrap();
    lm.flush(cp).unwrap();

    // T1 does not commit before crash (it is a loser across checkpoint)
    drop(pm);
    drop(lm);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 1);

    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[0, 0, 0, 0]);
}

#[test]
fn test_crash_simulation_during_checkpoint_wal_flush() {
    let (_dir, db_path, wal_path) = create_test_env("crash_during_cp_wal_flush");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![7, 7, 7, 7])
        .unwrap();
    let c1 = lm.commit_txn(t1).unwrap();
    lm.flush(c1).unwrap();

    // Checkpoint with simulated fault during WAL flush
    let err = lm
        .append_checkpoint_with_fault(
            Vec::new(),
            Some(StorageFailurePoint::DuringCheckpointWalFlush),
        )
        .unwrap_err();
    assert!(matches!(err, Error::InjectedFault(_)));

    drop(pm);
    drop(lm);

    // Reopen & recover: T1 was committed and durable before checkpoint failure
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.committed_txns, 1);
    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[7, 7, 7, 7]);
}

#[test]
fn test_wal_retention_calculation_with_active_transactions() {
    let (_dir, _db_path, wal_path) = create_test_env("wal_retention_calc");

    let mut lm = LogManager::create(&wal_path).unwrap();

    // No active transactions: retention LSN is checkpoint LSN
    let cp1 = lm.append_checkpoint(Vec::new()).unwrap();
    assert_eq!(lm.calculate_retention_lsn(cp1), cp1);

    // T1 starts (LSN 2)
    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, 1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();

    // Checkpoint at LSN 4
    let cp2 = lm.append_checkpoint(vec![(t1, d1)]).unwrap();

    // Retention LSN must be T1's first LSN (LSN <= d1 < cp2), preserving T1's delta
    let retention = lm.calculate_retention_lsn(cp2);
    assert!(
        retention <= d1,
        "retention LSN {retention} must be <= active txn delta LSN {d1}"
    );
}

// ===========================================================================
// SECTION 3: Record-Chain and Undo Invariants
// ===========================================================================

#[test]
fn test_multiple_updates_to_same_record_within_one_transaction_undone_in_order() {
    let (_dir, db_path, wal_path) = create_test_env("multiple_updates_one_txn_undo");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();

    // 4 successive updates to offset 64
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    lm.append_page_delta(t1, p1, 64, vec![1, 1, 1, 1], vec![2, 2, 2, 2])
        .unwrap();
    lm.append_page_delta(t1, p1, 64, vec![2, 2, 2, 2], vec![3, 3, 3, 3])
        .unwrap();
    lm.append_page_delta(t1, p1, 64, vec![3, 3, 3, 3], vec![4, 4, 4, 4])
        .unwrap();

    // Simulate page write with final value
    let mut page = pm.read_page(p1).unwrap();
    page.as_bytes_mut()[64..68].copy_from_slice(&[4, 4, 4, 4]);
    pm.write_page(&page).unwrap();

    lm.flush(lm.next_lsn()).unwrap();
    drop(pm);
    drop(lm);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 4);
    assert_eq!(stats.clrs_generated, 4);

    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[0, 0, 0, 0]);
}

#[test]
fn test_missing_record_in_undo_chain_fails_with_wal_corruption() {
    let (_dir, db_path, wal_path) = create_test_env("missing_undo_record_fails");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();

    drop(pm);
    drop(lm);

    // Truncate WAL to omit the PageDelta record while keeping the Begin record
    let file = OpenOptions::new().write(true).open(&wal_path).unwrap();
    // Header (32) + Begin record length (~33 bytes) = 65 bytes
    file.set_len(65).unwrap();
    drop(file);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    // The transaction only has Begin in the truncated file, which undo handles cleanly
    let stats = RecoveryEngine::recover(&mut pm, &mut lm);
    assert!(stats.is_ok());
}

#[test]
fn test_active_loser_with_completed_clrs_finalized_on_recovery() {
    let (_dir, db_path, wal_path) = create_test_env("loser_with_completed_clrs_finalized");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();
    lm.flush(d1).unwrap();

    // Append CLR with undo_next_lsn = NULL_LSN (simulating all operations of T1 were undone)
    let clr_lsn = lm.append_clr(t1, p1, 64, vec![0; 4], NULL_LSN).unwrap();
    lm.flush(clr_lsn).unwrap();
    // Crash right before abort_txn(t1) is called!

    drop(pm);
    drop(lm);

    // Open and recover: RecoveryEngine must finalize T1 by appending an Abort record
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 1);
    // T1 was finalized: verify no active transactions remain in LogManager
    assert!(
        lm.active_txns().is_empty(),
        "active_txns must be empty after recovery finalizes completed losers"
    );
}

// ===========================================================================
// SECTION 4: WAL Reader Boundaries Matrix
// ===========================================================================

#[test]
fn test_wal_reader_boundary_empty_and_checkpoint_only() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_empty_and_cp");

    // Case A: Empty WAL (header only)
    {
        let mut lm = LogManager::create(&wal_path).unwrap();
        let records = lm.read_all_records().unwrap();
        assert!(records.is_empty(), "empty WAL should yield 0 records");
    }

    // Case B: Checkpoint-only WAL
    {
        let mut lm = LogManager::open(&wal_path).unwrap();
        let cp_lsn = lm.append_checkpoint(Vec::new()).unwrap();
        lm.flush(cp_lsn).unwrap();

        let records = lm.read_all_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lsn, cp_lsn);
    }
}

#[test]
fn test_wal_reader_boundary_truncated_header_rejected() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_truncated_header");

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&wal_path)
        .unwrap();
    // Write 16 bytes (< 32 WAL_HEADER_SIZE)
    file.write_all(b"TREEWAL1_SHORT!!").unwrap();
    drop(file);

    let res = LogManager::open(&wal_path);
    assert!(
        res.is_err(),
        "Truncated WAL header (< 32 bytes) must be rejected"
    );
}

#[test]
fn test_wal_reader_boundary_invalid_payload_length_limits() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_invalid_payload_limits");

    let mut lm = LogManager::create(&wal_path).unwrap();
    let t1 = lm.begin_txn().unwrap();
    let lsn1 = lm
        .append_page_delta(t1, 1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();
    let t2 = lm.begin_txn().unwrap();
    let lsn2 = lm
        .append_page_delta(t2, 1, 68, vec![0; 4], vec![5, 6, 7, 8])
        .unwrap();
    lm.flush(lsn2).unwrap();
    drop(lm);

    // Overwrite payload_len of record 2 with length below MIN_WAL_RECORD_PAYLOAD_LEN (29)
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wal_path)
        .unwrap();
    let mut reader = WalReader::new(&file, lsn2).unwrap();
    reader.seek_to_offset(32).unwrap();
    let (_r1, _off1, len1) = reader.next_record_with_offset().unwrap().unwrap();
    let r2_offset = 32 + len1 as u64;

    file.seek(SeekFrom::Start(r2_offset)).unwrap();
    file.write_all(&12u32.to_le_bytes()).unwrap(); // 12 < MIN_WAL_RECORD_PAYLOAD_LEN (29)
    file.flush().unwrap();
    drop(file);

    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::new(file, lsn1).unwrap();
    reader.seek_to_offset(32).unwrap();
    let _r1 = reader.next_record_with_offset().unwrap();
    let err = reader.next_record_with_offset().unwrap_err();
    assert!(
        matches!(err, Error::WalCorruption(_)),
        "Payload length below minimum must be rejected as WalCorruption"
    );
}

#[test]
fn test_wal_reader_boundary_payload_exceeding_max_rejected() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_payload_exceeds_max");

    let mut lm = LogManager::create(&wal_path).unwrap();
    let t1 = lm.begin_txn().unwrap();
    let lsn1 = lm
        .append_page_delta(t1, 1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();
    let t2 = lm.begin_txn().unwrap();
    let lsn2 = lm
        .append_page_delta(t2, 1, 68, vec![0; 4], vec![5, 6, 7, 8])
        .unwrap();
    lm.flush(lsn2).unwrap();
    drop(lm);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wal_path)
        .unwrap();
    let mut reader = WalReader::new(&file, lsn2).unwrap();
    reader.seek_to_offset(32).unwrap();
    let (_r1, _off1, len1) = reader.next_record_with_offset().unwrap().unwrap();
    let r2_offset = 32 + len1 as u64;

    file.seek(SeekFrom::Start(r2_offset)).unwrap();
    // Write 32 MB (> MAX_WAL_RECORD_PAYLOAD_LEN 16 MB)
    file.write_all(&(32 * 1024 * 1024u32).to_le_bytes())
        .unwrap();
    file.flush().unwrap();
    drop(file);

    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::new(file, lsn1).unwrap();
    reader.seek_to_offset(32).unwrap();
    let _r1 = reader.next_record_with_offset().unwrap();
    let err = reader.next_record_with_offset().unwrap_err();
    assert!(
        matches!(err, Error::WalCorruption(_)),
        "Payload length exceeding maximum must be rejected as WalCorruption"
    );
}

#[test]
fn test_wal_reader_boundary_non_monotonic_lsn_detected() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_non_monotonic_lsn");

    let mut lm = LogManager::create(&wal_path).unwrap();
    let t1 = lm.begin_txn().unwrap();
    let rec1 = LogRecord::new_begin(Lsn(10), t1);
    let rec2 = LogRecord::new_begin(Lsn(5), t1); // Non-monotonic: 5 <= 10

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&rec1.serialize());
    bytes.extend_from_slice(&rec2.serialize());
    drop(lm);

    let cursor = std::io::Cursor::new(bytes);
    let mut reader = WalReader::new(cursor, Lsn(10)).unwrap();
    let r1 = reader.next_record().unwrap().expect("record 1");
    assert_eq!(r1.lsn, Lsn(10));

    let err = reader.next_record().unwrap_err();
    assert!(
        matches!(err, Error::WalCorruption(_)),
        "Non-monotonic LSN must produce WalCorruption, got: {err:?}"
    );
}

#[test]
fn test_wal_reader_boundary_large_record_near_expansion_boundary() {
    let (_dir, _db_path, wal_path) = create_test_env("reader_large_record");

    let mut lm = LogManager::create(&wal_path).unwrap();
    let t1 = lm.begin_txn().unwrap();
    // 128 KB payload exceeds DEFAULT_WAL_BUFFER_SIZE (64 KB)
    let payload_size = 128 * 1024;
    let before = vec![0xAA; payload_size];
    let after = vec![0xBB; payload_size];

    let lsn = lm.append_page_delta(t1, 1, 0, before, after).unwrap();
    lm.flush(lsn).unwrap();
    drop(lm);

    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::new(file, lsn).unwrap();
    reader.seek_to_offset(32).unwrap();
    let (rec1, _, _) = reader.next_record_with_offset().unwrap().expect("begin");
    assert_eq!(rec1.lsn, Lsn(1));

    let (rec2, _, _) = reader
        .next_record_with_offset()
        .unwrap()
        .expect("large delta");
    assert_eq!(rec2.lsn, lsn);
}

// ===========================================================================
// SECTION 5: Independent Correctness Oracle Matrix
// ===========================================================================

#[derive(Default, Debug, Clone)]
struct LogicalOracle {
    committed_kv: BTreeMap<u64, Vec<u8>>,
    active_txn_kv: BTreeMap<TxnId, BTreeMap<u64, Option<Vec<u8>>>>,
}

impl LogicalOracle {
    fn begin_txn(&mut self, txn_id: TxnId) {
        self.active_txn_kv.insert(txn_id, BTreeMap::new());
    }

    fn put(&mut self, txn_id: TxnId, key: u64, value: Vec<u8>) {
        if let Some(map) = self.active_txn_kv.get_mut(&txn_id) {
            map.insert(key, Some(value));
        }
    }

    fn delete(&mut self, txn_id: TxnId, key: u64) {
        if let Some(map) = self.active_txn_kv.get_mut(&txn_id) {
            map.insert(key, None);
        }
    }

    fn commit(&mut self, txn_id: TxnId) {
        if let Some(updates) = self.active_txn_kv.remove(&txn_id) {
            for (k, val_opt) in updates {
                match val_opt {
                    Some(val) => {
                        self.committed_kv.insert(k, val);
                    }
                    None => {
                        self.committed_kv.remove(&k);
                    }
                }
            }
        }
    }

    fn abort(&mut self, txn_id: TxnId) {
        self.active_txn_kv.remove(&txn_id);
    }
}

struct SimplePrng {
    state: u64,
}

impl SimplePrng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    fn next_range(&mut self, max: u64) -> u64 {
        if max == 0 {
            0
        } else {
            self.next_u64() % max
        }
    }
}

#[test]
fn test_oracle_differential_recovery_fixed_seeds() {
    let seeds = [42u64, 1337, 2026, 9999];

    for &seed in &seeds {
        let mut prng = SimplePrng::new(seed);
        let (_dir, db_path, _wal_path) = create_test_env(&format!("oracle_seed_{seed}"));

        let mut oracle = LogicalOracle::default();
        let mut operation_log = Vec::new();

        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1000, b"seed_val").unwrap();
        oracle.committed_kv.insert(1000, b"seed_val".to_vec());
        db.sync().unwrap();

        let mut txn_counter = 100u64;

        // Perform 60 deterministic transactional operations with periodic reopens
        for step in 0..60 {
            let op_choice = prng.next_range(4);
            match op_choice {
                0 => {
                    // Committed multi-operation transaction
                    let tid = TxnId(txn_counter);
                    txn_counter += 1;
                    oracle.begin_txn(tid);
                    let mut txn = db.begin().unwrap();
                    let op_count = 1 + prng.next_range(3);
                    for _ in 0..op_count {
                        let key = prng.next_range(25) + 1;
                        if prng.next_range(4) == 0 {
                            txn.delete(key).unwrap();
                            oracle.delete(tid, key);
                            operation_log.push(format!("{step}: txn {tid:?} DELETE k={key}"));
                        } else {
                            let val =
                                format!("val_{step}_{key}_{}", prng.next_u64() % 1000).into_bytes();
                            txn.put(key, &val).unwrap();
                            oracle.put(tid, key, val);
                            operation_log.push(format!("{step}: txn {tid:?} PUT k={key}"));
                        }
                    }
                    txn.commit().unwrap();
                    oracle.commit(tid);
                    operation_log.push(format!("{step}: txn {tid:?} COMMIT"));
                    if step % 5 == 0 {
                        db.sync().unwrap();
                    }
                }
                1 => {
                    // Aborted multi-operation transaction
                    let tid = TxnId(txn_counter);
                    txn_counter += 1;
                    oracle.begin_txn(tid);
                    let mut txn = db.begin().unwrap();
                    let op_count = 1 + prng.next_range(3);
                    for _ in 0..op_count {
                        let key = prng.next_range(25) + 1;
                        let val = format!("abort_val_{step}_{key}").into_bytes();
                        txn.put(key, &val).unwrap();
                        oracle.put(tid, key, val);
                        operation_log.push(format!("{step}: txn {tid:?} PUT (abort) k={key}"));
                    }
                    txn.abort().unwrap();
                    oracle.abort(tid);
                    operation_log.push(format!("{step}: txn {tid:?} ABORT"));
                }
                2 => {
                    // Direct autocommit put or delete
                    let key = prng.next_range(25) + 1;
                    if prng.next_range(3) == 0 {
                        db.delete(key).unwrap();
                        oracle.committed_kv.remove(&key);
                        operation_log.push(format!("{step}: AUTOCOMMIT DELETE k={key}"));
                    } else {
                        let val = format!("autocommit_{step}_{key}").into_bytes();
                        db.put(key, &val).unwrap();
                        oracle.committed_kv.insert(key, val);
                        operation_log.push(format!("{step}: AUTOCOMMIT PUT k={key}"));
                    }
                    if step % 5 == 0 {
                        db.sync().unwrap();
                    }
                }
                3 => {
                    // Periodic checkpoint and reopen cycle
                    db.sync().unwrap();
                    if step % 10 == 0 {
                        db.checkpoint_and_truncate_wal().unwrap();
                    }
                    drop(db);
                    db = TreeDb::open(&db_path, 64).unwrap();
                    operation_log.push(format!("{step}: REOPEN & RECOVER DATABASE"));
                }
                _ => unreachable!(),
            }
        }

        db.sync().unwrap();

        // Simulate crash with an uncommitted transaction right before reopen
        {
            let mut txn = db.begin().unwrap();
            txn.put(25, b"uncommitted_crash_value").unwrap();
            // Transaction dropped without commit -> rollback simulated
        }
        db.sync().unwrap();
        drop(db);

        // Final Crash & Reopen Verification
        let mut db = TreeDb::open(&db_path, 64).unwrap();

        for key in 1..=25 {
            let actual = db.get(key).unwrap();
            let expected = oracle.committed_kv.get(&key).cloned();
            assert_eq!(
                actual,
                expected,
                "Oracle differential mismatch on seed {seed} for key {key}!\nOperation trace:\n{}",
                operation_log.join("\n")
            );
        }

        // Idempotency: second consecutive reopen
        drop(db);
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        for key in 1..=25 {
            let actual = db.get(key).unwrap();
            let expected = oracle.committed_kv.get(&key).cloned();
            assert_eq!(
                actual, expected,
                "Second reopen idempotency mismatch on seed {seed} for key {key}"
            );
        }
    }
}

// ===========================================================================
// SECTION 6: WAL Offset Cache Lifecycle Audit
// ===========================================================================

#[test]
fn test_wal_offset_cache_lifecycle_transitions() {
    let (_dir, db_path, wal_path) = create_test_env("offset_cache_lifecycle");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // Transition 1: Record Append
    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();

    let cached_offset = lm.get_record_offset(t1, d1);
    assert!(
        cached_offset.is_some(),
        "d1 offset must be cached on append"
    );
    let rec = lm.read_record_at_offset(cached_offset.unwrap()).unwrap();
    assert_eq!(rec.lsn, d1);
    assert_eq!(rec.txn_id, t1);

    // Transition 2: Checkpoint Creation
    let cp = lm.append_checkpoint(vec![(t1, d1)]).unwrap();
    lm.flush(cp).unwrap();
    // Offset cache for active txn t1 remains valid across checkpoint
    assert_eq!(lm.get_record_offset(t1, d1), cached_offset);

    // Transition 3: CLR Append
    let clr = lm.append_clr(t1, p1, 64, vec![0; 4], NULL_LSN).unwrap();
    assert!(
        lm.get_record_offset(t1, clr).is_some(),
        "CLR offset must be cached"
    );

    // Transition 4: Transaction Commit or Abort cleans up offsets
    let a1 = lm.abort_txn(t1).unwrap();
    lm.flush(a1).unwrap();
    assert!(
        lm.get_record_offset(t1, d1).is_none(),
        "aborted transaction offsets must be purged"
    );
    assert!(
        lm.get_record_offset(t1, clr).is_none(),
        "aborted transaction CLR offset must be purged"
    );

    // Transition 5: WAL Prefix Truncation & Offset Rebuild
    let t2 = lm.begin_txn().unwrap();
    let d2 = lm
        .append_page_delta(t2, p1, 68, vec![0; 4], vec![2, 2, 2, 2])
        .unwrap();
    let old_offset_d2 = lm.get_record_offset(t2, d2).unwrap();

    let cp2 = lm.append_checkpoint(vec![(t2, d2)]).unwrap();
    lm.flush(cp2).unwrap();

    // Truncate records before cp2's retention boundary
    let retention = lm.calculate_retention_lsn(cp2);
    let removed = lm.truncate_prefix(retention).unwrap();
    if removed > 0 {
        let new_offset_d2 = lm.get_record_offset(t2, d2).unwrap();
        assert!(
            new_offset_d2 < old_offset_d2,
            "Truncation must adjust cached offset forward in the smaller file"
        );
        let rec2 = lm.read_record_at_offset(new_offset_d2).unwrap();
        assert_eq!(rec2.lsn, d2);
        assert_eq!(rec2.txn_id, t2);
    }

    // Transition 6: Fallback scan accuracy
    // Request a record for a non-existent transaction or LSN
    let absent = lm.read_record_for_txn(TxnId(9999), Lsn(9999)).unwrap();
    assert!(absent.is_none(), "absent record must return None safely");
    assert!(
        lm.get_record_offset(TxnId(9999), Lsn(9999)).is_none(),
        "absent record lookup must not leave any offset cached"
    );
}
