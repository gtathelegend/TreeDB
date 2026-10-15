#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use tempfile::tempdir;
use treedb::{
    wal::lsn::NULL_LSN, Error, LogManager, PageManager, RecoveryEngine, RecoveryFault, TreeDb,
    WalReader,
};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

// ---------------------------------------------------------------------------
// 1. An active transaction spanning a checkpoint
// ---------------------------------------------------------------------------
#[test]
fn test_active_transaction_spanning_checkpoint() {
    let (_dir, db_path, wal_path) = create_test_env("span_cp");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 begins before checkpoint
    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();

    // Checkpoint with T1 active
    let cp_lsn = lm.append_checkpoint(vec![(t1, lm.next_lsn())]).unwrap();
    lm.flush(cp_lsn).unwrap();

    // T1 continues after checkpoint
    lm.append_page_delta(t1, p1, 68, vec![0; 4], vec![5, 6, 7, 8])
        .unwrap();
    let commit_lsn = lm.commit_txn(t1).unwrap();
    lm.flush(commit_lsn).unwrap();

    drop(pm);
    drop(lm);

    // Reopen and recover
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.committed_txns, 1);
    assert_eq!(stats.loser_txns, 0);

    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[1, 2, 3, 4]);
    assert_eq!(&page.as_bytes()[68..72], &[5, 6, 7, 8]);
}

// ---------------------------------------------------------------------------
// 2. Transaction abort after checkpointing
// ---------------------------------------------------------------------------
#[test]
fn test_transaction_abort_after_checkpointing() {
    let (_dir, db_path, wal_path) = create_test_env("abort_after_cp");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // Write base data at page body (offset 64)
    let t0 = lm.begin_txn().unwrap();
    lm.append_page_delta(t0, p1, 64, vec![0; 8], vec![9, 9, 9, 9, 9, 9, 9, 9])
        .unwrap();
    let c0 = lm.commit_txn(t0).unwrap();
    lm.flush(c0).unwrap();

    // Mutate page in memory
    let mut page = pm.read_page(p1).unwrap();
    page.as_bytes_mut()[64..72].copy_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
    pm.write_page(&page).unwrap();

    // T1 begins and modifies page
    let t1 = lm.begin_txn().unwrap();
    let delta_lsn = lm
        .append_page_delta(t1, p1, 64, vec![9; 8], vec![1, 1, 1, 1, 1, 1, 1, 1])
        .unwrap();
    page.as_bytes_mut()[64..72].copy_from_slice(&[1, 1, 1, 1, 1, 1, 1, 1]);
    pm.write_page(&page).unwrap();

    // Checkpoint occurs while T1 is active
    let cp_lsn = lm.append_checkpoint(vec![(t1, delta_lsn)]).unwrap();
    lm.flush(cp_lsn).unwrap();

    // T1 aborts after checkpoint: read_record_for_txn is invoked during rollback
    let rec = lm.read_record_for_txn(t1, delta_lsn).unwrap();
    assert!(rec.is_some(), "T1 delta record must be retrievable");

    let clr_lsn = lm.append_clr(t1, p1, 64, vec![9; 8], NULL_LSN).unwrap();
    lm.flush(clr_lsn).unwrap();
    let abort_lsn = lm.abort_txn(t1).unwrap();
    lm.flush(abort_lsn).unwrap();

    // Restore page before image
    page.as_bytes_mut()[64..72].copy_from_slice(&[9; 8]);
    page.set_lsn(clr_lsn);
    pm.write_page(&page).unwrap();

    drop(pm);
    drop(lm);

    // Recover and verify
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 0);
    let recovered_page = pm.read_page(p1).unwrap();
    assert_eq!(&recovered_page.as_bytes()[64..72], &[9; 8]);
}

// ---------------------------------------------------------------------------
// 3. WAL prefix truncation while transaction-related metadata exists
// ---------------------------------------------------------------------------
#[test]
fn test_wal_prefix_truncation_with_stale_offset_handling() {
    let (_dir, db_path, wal_path) = create_test_env("prefix_truncation");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 performs write
    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![10, 11, 12, 13])
        .unwrap();

    // Record offset of d1 prior to commit and truncation
    let recorded_offset = lm.get_record_offset(t1, d1);
    assert!(recorded_offset.is_some());

    let c1 = lm.commit_txn(t1).unwrap();
    lm.flush(c1).unwrap();

    // Checkpoint
    let cp = lm.append_checkpoint(Vec::new()).unwrap();
    lm.flush(cp).unwrap();

    // Truncate prefix
    let truncated_bytes = lm.truncate_prefix(cp).unwrap();
    assert!(truncated_bytes > 0, "bytes should be truncated");

    // Reading truncated record via read_record_for_txn safely returns Ok(None)
    let res = lm.read_record_for_txn(t1, d1);
    assert!(res.is_ok());
    assert!(res.unwrap().is_none());

    // Now start a new transaction T2 after truncation
    let t2 = lm.begin_txn().unwrap();
    let _d2 = lm
        .append_page_delta(t2, p1, 68, vec![0; 4], vec![20, 21, 22, 23])
        .unwrap();
    let c2 = lm.commit_txn(t2).unwrap();
    lm.flush(c2).unwrap();

    drop(pm);
    drop(lm);

    // Reopen and recover cleanly
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    assert_eq!(stats.loser_txns, 0);
}

// ---------------------------------------------------------------------------
// 4. Reopen and recovery after checkpoint and truncation
// ---------------------------------------------------------------------------
#[test]
fn test_reopen_recovery_after_checkpoint_and_truncation() {
    let (_dir, db_path, _wal_path) = create_test_env("reopen_cp_trunc");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        for i in 0..100 {
            db.put(i, &(i * 3).to_le_bytes()).unwrap();
            if i % 25 == 0 {
                db.checkpoint_and_truncate_wal().unwrap();
            }
        }
        db.sync().unwrap();
    }

    // Reopen
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    for i in 0..100 {
        let val = db.get(i).unwrap().expect("key must exist");
        assert_eq!(val, (i * 3).to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// 5. Multiple transactions with interleaved records and undo chains
// ---------------------------------------------------------------------------
#[test]
fn test_multiple_txns_interleaved_undo_chains() {
    let (_dir, db_path, wal_path) = create_test_env("interleaved_undo");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let p2 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    let t2 = lm.begin_txn().unwrap();
    let t3 = lm.begin_txn().unwrap();

    // Interleave writes in page body (offset >= 64)
    // T1 write to p1
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    // T2 write to p2
    lm.append_page_delta(t2, p2, 64, vec![0; 4], vec![2, 2, 2, 2])
        .unwrap();
    // T1 second write to p1
    lm.append_page_delta(t1, p1, 68, vec![0; 4], vec![3, 3, 3, 3])
        .unwrap();
    // T3 write to p2
    lm.append_page_delta(t3, p2, 68, vec![0; 4], vec![4, 4, 4, 4])
        .unwrap();

    // T2 commits (winner)
    let c2 = lm.commit_txn(t2).unwrap();
    lm.flush(c2).unwrap();

    // T1 and T3 are uncommitted losers at simulated crash
    drop(pm);
    drop(lm);

    // Recover
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    // Loser txns: T1 and T3 (2 losers)
    assert_eq!(stats.loser_txns, 2);
    assert_eq!(stats.committed_txns, 1);
    // T1 had 2 deltas, T3 had 1 delta -> total 3 undos
    assert_eq!(stats.undo_count, 3);
    assert_eq!(stats.clrs_generated, 3);

    // Verify p1 before-images were restored (zeros)
    let page1 = pm.read_page(p1).unwrap();
    assert_eq!(&page1.as_bytes()[64..72], &[0; 8]);

    // Verify p2: T2's write at 64..68 is preserved ([2,2,2,2]), T3's write at 68..72 is restored to 0
    let page2 = pm.read_page(p2).unwrap();
    assert_eq!(&page2.as_bytes()[64..68], &[2, 2, 2, 2]);
    assert_eq!(&page2.as_bytes()[68..72], &[0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// 6. Repeated recovery when compensation log records have already been appended
// ---------------------------------------------------------------------------
#[test]
fn test_repeated_recovery_with_existing_clrs() {
    let (_dir, db_path, wal_path) = create_test_env("repeated_recovery");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![10, 10, 10, 10])
        .unwrap();
    lm.append_page_delta(t1, p1, 68, vec![0; 4], vec![20, 20, 20, 20])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();
    drop(pm);
    drop(lm);

    // Recovery 1: Crash after 1 recovered page write (1 CLR written & flushed)
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let err = RecoveryEngine::recover_with_fault(
            &mut pm,
            &mut lm,
            Some(RecoveryFault::AfterRecoveredPageWrite(1)),
        )
        .unwrap_err();
        assert!(matches!(err, Error::RecoveryInterrupted(_)));
    }

    // Recovery 2: Run to completion. It must see the CLR, not re-undo the first delta,
    // and successfully undo the remaining delta.
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

        // Second pass should only need 1 undo for the remaining un-undone delta
        assert_eq!(stats.undo_count, 1);
        assert_eq!(stats.clrs_generated, 1);
        assert_eq!(stats.loser_txns, 1);

        let page = pm.read_page(p1).unwrap();
        assert_eq!(&page.as_bytes()[64..72], &[0; 8]);
    }

    // Recovery 3: Third recovery on the now clean database. No losers, 0 undos!
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 0);
        assert_eq!(stats.undo_count, 0);
    }
}

// ---------------------------------------------------------------------------
// 7. Failure injection around checkpoint and truncation boundaries
// ---------------------------------------------------------------------------
#[test]
fn test_failure_injection_around_checkpoint_and_truncation() {
    let (_dir, db_path, wal_path) = create_test_env("cp_fault_injection");

    // Scenario A: Crash before checkpoint is flushed
    {
        let mut pm = PageManager::create(&db_path).unwrap();
        let p1 = pm.allocate_page().unwrap();
        let mut lm = LogManager::create(&wal_path).unwrap();
        let t1 = lm.begin_txn().unwrap();
        lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![7, 7, 7, 7])
            .unwrap();
        let c1 = lm.commit_txn(t1).unwrap();
        lm.flush(c1).unwrap();

        // Checkpoint written to WAL buffer but NOT flushed
        let _cp = lm.append_checkpoint(Vec::new()).unwrap();
        drop(pm);
        drop(lm);

        // Recover: T1 should still be replayed properly regardless of checkpoint flush
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.committed_txns, 1);
        let page = pm.read_page(p1).unwrap();
        assert_eq!(&page.as_bytes()[64..68], &[7, 7, 7, 7]);
    }
}

// ---------------------------------------------------------------------------
// 8. WAL corruption and partial-record validation
// ---------------------------------------------------------------------------
#[test]
fn test_wal_mid_stream_invalid_payload_length_rejected() {
    let (_dir, _db_path, wal_path) = create_test_env("invalid_payload_mid");

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

    // Overwrite the payload_len field of record 2 with an invalid tiny value (e.g. 5 < 29)
    // while valid data still follows in the file.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wal_path)
        .unwrap();

    // Scan with reader to get offset of record 2 (seek past 32-byte header)
    let mut reader = WalReader::new(&file, lsn2).unwrap();
    reader.seek_to_offset(32).unwrap();
    let (_r1, _off1, len1) = reader.next_record_with_offset().unwrap().unwrap();
    let r2_offset = 32 + len1 as u64;

    file.seek(SeekFrom::Start(r2_offset)).unwrap();
    // Write corrupted payload_len: 10 bytes (< MIN_WAL_RECORD_PAYLOAD_LEN 29)
    file.write_all(&10u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
    drop(file);

    // Reading through WalReader should detect corruption and return Err(WalCorruption)
    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::new(file, lsn1).unwrap();
    reader.seek_to_offset(32).unwrap();
    let _r1 = reader.next_record_with_offset().unwrap();
    let err = reader.next_record_with_offset().unwrap_err();
    assert!(
        matches!(err, Error::WalCorruption(_)),
        "Mid-log invalid payload length must be rejected as WalCorruption, got: {err:?}"
    );
}

#[test]
fn test_wal_trailing_partial_write_accepted_if_unflushed() {
    let (_dir, _db_path, wal_path) = create_test_env("trailing_partial");

    let mut lm = LogManager::create(&wal_path).unwrap();
    let t1 = lm.begin_txn().unwrap();
    let lsn1 = lm
        .append_page_delta(t1, 1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();
    lm.flush(lsn1).unwrap();
    drop(lm);

    // Append 10 bytes of partial junk to end of WAL file to simulate crash mid-write of unflushed record
    let mut file = OpenOptions::new().append(true).open(&wal_path).unwrap();
    file.write_all(&[0xFF; 10]).unwrap();
    file.flush().unwrap();
    drop(file);

    // Reading with flushed_lsn = lsn1: partial write at EOF past flushed_lsn should return Ok(None)
    let file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let mut reader = WalReader::new(file, lsn1).unwrap();
    reader.seek_to_offset(32).unwrap();
    let r1 = reader.next_record_with_offset().unwrap();
    assert!(r1.is_some(), "Begin record");
    let r2 = reader.next_record_with_offset().unwrap();
    assert!(r2.is_some(), "PageDelta record");
    let eof = reader.next_record_with_offset().unwrap();
    assert!(
        eof.is_none(),
        "Unflushed trailing fragment should be safely ignored"
    );
}
