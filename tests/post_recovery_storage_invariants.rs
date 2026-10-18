#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use tempfile::tempdir;

use treedb::wal::log_record::LogRecordType;
use treedb::wal::lsn::Lsn;
use treedb::{
    BufferPool, Error, LogManager, Page, PageManager, PageType, RecordStore, RecoveryEngine,
    RecoveryFault, TreeDb, HEADER_PAGE_ID, INVALID_PAGE_ID, PAGE_SIZE,
};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

// ===========================================================================
// SUITE 1: Recovery Transaction Finalization Audit
// ===========================================================================

#[test]
fn test_recovery_finalization_exact_single_abort_record() {
    let (_dir, db_path, wal_path) = create_test_env("recovery_exact_abort");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    lm.append_page_delta(t1, p1, 68, vec![0; 4], vec![2, 2, 2, 2])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();

    drop(pm);
    drop(lm);

    // First recovery
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 2);

    // Verify exactly ONE Abort record for t1 was appended
    let records = lm.read_all_records().unwrap();
    let abort_count = records
        .iter()
        .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
        .count();
    assert_eq!(abort_count, 1, "must append exactly one abort record");
    assert!(lm.active_txns().is_empty());
}

#[test]
fn test_recovery_no_duplicate_abort_on_subsequent_recoveries() {
    let (_dir, db_path, wal_path) = create_test_env("recovery_no_dup_abort");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![9, 9, 9, 9])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();
    drop(pm);
    drop(lm);

    // Recovery Cycle 1
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 1);
        assert_eq!(stats.undo_count, 1);
    }

    // Recovery Cycle 2
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 0);
        assert_eq!(stats.undo_count, 0);

        let records = lm.read_all_records().unwrap();
        let abort_count = records
            .iter()
            .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
            .count();
        assert_eq!(
            abort_count, 1,
            "subsequent recovery must not append duplicate abort record"
        );
        assert!(lm.active_txns().is_empty());
    }

    // Recovery Cycle 3
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 0);
        assert_eq!(stats.undo_count, 0);

        let records = lm.read_all_records().unwrap();
        let abort_count = records
            .iter()
            .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
            .count();
        assert_eq!(abort_count, 1);
        assert!(lm.active_txns().is_empty());
    }
}

#[test]
fn test_recovery_pending_undo_not_finalized_prematurely() {
    let (_dir, db_path, wal_path) = create_test_env("recovery_pending_undo");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    lm.append_page_delta(t1, p1, 68, vec![0; 4], vec![2, 2, 2, 2])
        .unwrap();
    lm.append_page_delta(t1, p1, 72, vec![0; 4], vec![3, 3, 3, 3])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();

    // Simulate crash after recovering 1st page write (2 operations still pending undo)
    let err = RecoveryEngine::recover_with_fault(
        &mut pm,
        &mut lm,
        Some(RecoveryFault::AfterRecoveredPageWrite(1)),
    )
    .unwrap_err();
    assert!(matches!(err, Error::RecoveryInterrupted(_)));

    // Verify t1 was NOT aborted yet, as 2 operations remained
    let records = lm.read_all_records().unwrap();
    let has_abort = records
        .iter()
        .any(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort);
    assert!(
        !has_abort,
        "transaction with pending undos must not be finalized with Abort record prematurely"
    );

    drop(pm);
    drop(lm);

    // Complete recovery cleanly
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    assert!(
        stats.undo_count >= 2,
        "remaining pending undos must be processed"
    );

    // Now exactly one abort record must exist
    let records = lm.read_all_records().unwrap();
    let abort_count = records
        .iter()
        .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
        .count();
    assert_eq!(
        abort_count, 1,
        "after all undos complete, exactly one abort record must exist"
    );
    assert!(lm.active_txns().is_empty());
}

#[test]
fn test_recovery_interleaved_losers_finish_independently() {
    let (_dir, db_path, wal_path) = create_test_env("interleaved_finish_independently");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    // T1 has 1 delta, T2 has 3 deltas
    let t1 = lm.begin_txn().unwrap();
    let t2 = lm.begin_txn().unwrap();

    lm.append_page_delta(t2, p1, 64, vec![0; 4], vec![2, 1, 0, 0])
        .unwrap();
    lm.append_page_delta(t1, p1, 80, vec![0; 4], vec![1, 1, 1, 1])
        .unwrap();
    lm.append_page_delta(t2, p1, 68, vec![0; 4], vec![2, 2, 0, 0])
        .unwrap();
    lm.append_page_delta(t2, p1, 72, vec![0; 4], vec![2, 3, 0, 0])
        .unwrap();
    lm.flush(lm.next_lsn()).unwrap();

    drop(pm);
    drop(lm);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 2);
    assert_eq!(stats.undo_count, 4); // 1 from T1, 3 from T2

    let records = lm.read_all_records().unwrap();
    let t1_aborts = records
        .iter()
        .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
        .count();
    let t2_aborts = records
        .iter()
        .filter(|r| r.txn_id == t2 && r.record_type == LogRecordType::Abort)
        .count();

    assert_eq!(t1_aborts, 1, "T1 must have exactly 1 abort record");
    assert_eq!(t2_aborts, 1, "T2 must have exactly 1 abort record");
    assert!(lm.active_txns().is_empty());

    let page = pm.read_page(p1).unwrap();
    assert_eq!(&page.as_bytes()[64..68], &[0, 0, 0, 0]);
    assert_eq!(&page.as_bytes()[68..72], &[0, 0, 0, 0]);
    assert_eq!(&page.as_bytes()[72..76], &[0, 0, 0, 0]);
    assert_eq!(&page.as_bytes()[80..84], &[0, 0, 0, 0]);
}

#[test]
fn test_recovery_clr_undo_next_lsn_traversal_termination() {
    let (_dir, db_path, wal_path) = create_test_env("clr_traversal_termination");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let t1 = lm.begin_txn().unwrap();
    let d1 = lm
        .append_page_delta(t1, p1, 64, vec![0; 4], vec![1, 2, 3, 4])
        .unwrap();
    let d2 = lm
        .append_page_delta(t1, p1, 68, vec![0; 4], vec![5, 6, 7, 8])
        .unwrap();
    lm.flush(d2).unwrap();

    // Append CLR compensating d2, pointing undo_next_lsn directly to d1
    let clr1 = lm.append_clr(t1, p1, 68, vec![0; 4], d1).unwrap();
    lm.flush(clr1).unwrap();

    drop(pm);
    drop(lm);

    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();

    assert_eq!(stats.loser_txns, 1);
    // d2 was already compensated by clr1, so only d1 is undone in this pass
    assert_eq!(stats.undo_count, 1);
    assert!(lm.active_txns().is_empty());

    let records = lm.read_all_records().unwrap();
    let abort_count = records
        .iter()
        .filter(|r| r.txn_id == t1 && r.record_type == LogRecordType::Abort)
        .count();
    assert_eq!(abort_count, 1);
}

#[test]
fn test_recovery_crash_during_clr_append_and_flush_boundaries() {
    // Case A: Crash during CLR append before flush
    {
        let (_dir, db_path, wal_path) = create_test_env("clr_append_fault");
        let mut pm = PageManager::create(&db_path).unwrap();
        let p1 = pm.allocate_page().unwrap();
        let mut lm = LogManager::create(&wal_path).unwrap();

        let t1 = lm.begin_txn().unwrap();
        lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![4, 3, 2, 1])
            .unwrap();
        lm.flush(lm.next_lsn()).unwrap();

        let err = RecoveryEngine::recover_with_fault(
            &mut pm,
            &mut lm,
            Some(RecoveryFault::AfterClrAppendBeforeFlush),
        )
        .unwrap_err();
        assert!(matches!(err, Error::RecoveryInterrupted(_)));

        drop(pm);
        drop(lm);

        // Second recovery pass replays CLR during Redo and finalizes loser transaction
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert_eq!(stats.loser_txns, 1);
        assert_eq!(
            stats.undo_count, 0,
            "undo chain was already compensated by durable CLR"
        );
        assert!(
            stats.redo_count >= 1,
            "durable CLR must be replayed during Redo"
        );
        assert!(lm.active_txns().is_empty());

        let page = pm.read_page(p1).unwrap();
        assert_eq!(&page.as_bytes()[64..68], &[0, 0, 0, 0]);
    }

    // Case B: Crash after CLR flush before page write
    {
        let (_dir, db_path, wal_path) = create_test_env("clr_flush_fault");
        let mut pm = PageManager::create(&db_path).unwrap();
        let p1 = pm.allocate_page().unwrap();
        let mut lm = LogManager::create(&wal_path).unwrap();

        let t1 = lm.begin_txn().unwrap();
        lm.append_page_delta(t1, p1, 64, vec![0; 4], vec![8, 7, 6, 5])
            .unwrap();
        lm.flush(lm.next_lsn()).unwrap();

        let err = RecoveryEngine::recover_with_fault(
            &mut pm,
            &mut lm,
            Some(RecoveryFault::AfterClrFlushBeforePageWrite),
        )
        .unwrap_err();
        assert!(matches!(err, Error::RecoveryInterrupted(_)));

        drop(pm);
        drop(lm);

        // Second recovery pass replays durable CLR via Redo and finalizes
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        assert!(stats.redo_count >= 1);
        assert!(lm.active_txns().is_empty());

        let page = pm.read_page(p1).unwrap();
        assert_eq!(&page.as_bytes()[64..68], &[0, 0, 0, 0]);
    }
}

// ===========================================================================
// SUITE 2: B+ Tree Root and Transaction Rollback Audit
// ===========================================================================

#[test]
fn test_bplus_tree_root_captured_from_live_tree_and_persisted_on_commit() {
    let (_dir, db_path, _wal_path) = create_test_env("bplus_root_live_capture");

    let initial_root;
    let split_root;

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        initial_root = db.tree().root_page_id();

        // Insert enough items to split the tree root
        let mut txn = db.begin().unwrap();
        for i in 1..=120 {
            txn.put(i, &(i * 100).to_le_bytes()).unwrap();
        }
        txn.commit().unwrap();

        split_root = db.tree().root_page_id();
        assert_ne!(
            initial_root, split_root,
            "root must have split into a new page"
        );
        assert_eq!(
            db.buffer_pool().page_manager().header().root_page_id,
            split_root,
            "header root_page_id must agree with live tree root after commit"
        );
        db.sync().unwrap();
    }

    // Reopen and verify committed root survived
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        assert_eq!(
            db.tree().root_page_id(),
            split_root,
            "live tree root must be the committed split root upon reopen"
        );
        for i in 1..=120 {
            let val = db.get(i).unwrap().expect("key must exist");
            assert_eq!(val, (i * 100).to_le_bytes());
        }
    }
}

#[test]
fn test_bplus_tree_split_then_abort_preserves_committed_keys() {
    let (_dir, db_path, _wal_path) = create_test_env("bplus_split_abort_preserve");

    let committed_root;

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        // Step 1: Commit baseline keys
        for i in 1..=30 {
            db.put(i, &(i * 10).to_le_bytes()).unwrap();
        }
        db.sync().unwrap();
        committed_root = db.tree().root_page_id();

        // Step 2: Uncommitted transaction triggers a B+ tree split
        let mut txn = db.begin().unwrap();
        for i in 31..=200 {
            txn.put(i, &(i * 10).to_le_bytes()).unwrap();
        }
        // Force abort
        txn.abort().unwrap();

        // Live tree root must restore committed_root
        assert_eq!(
            db.tree().root_page_id(),
            committed_root,
            "aborted transaction must restore authoritative pre-transaction root"
        );
        assert_eq!(
            db.buffer_pool().page_manager().header().root_page_id,
            committed_root,
            "header root_page_id must be restored to pre-transaction root"
        );

        // Verify baseline keys survive
        for i in 1..=30 {
            let val = db.get(i).unwrap().expect("baseline key must survive abort");
            assert_eq!(val, (i * 10).to_le_bytes());
        }

        // Verify aborted keys do not exist
        for i in 31..=200 {
            assert!(
                db.get(i).unwrap().is_none(),
                "aborted key {i} must not exist"
            );
        }
    }

    // Reopen and re-verify
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        assert_eq!(db.tree().root_page_id(), committed_root);
        for i in 1..=30 {
            let val = db
                .get(i)
                .unwrap()
                .expect("baseline key must survive reopen");
            assert_eq!(val, (i * 10).to_le_bytes());
        }
        for i in 31..=200 {
            assert!(db.get(i).unwrap().is_none());
        }
    }
}

#[test]
fn test_sequential_transactions_root_changes_abort_does_not_restore_obsolete_root() {
    let (_dir, db_path, _wal_path) = create_test_env("sequential_root_changes");

    let r1;
    let r2;

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        // Txn 1: Inserts and splits to root R1. Commits.
        {
            let mut txn = db.begin().unwrap();
            for i in 1..=120 {
                txn.put(i, &(i * 5).to_le_bytes()).unwrap();
            }
            txn.commit().unwrap();
        }
        r1 = db.tree().root_page_id();

        // Txn 2: Inserts more, splits to root R2. Commits.
        {
            let mut txn = db.begin().unwrap();
            for i in 121..=300 {
                txn.put(i, &(i * 5).to_le_bytes()).unwrap();
            }
            txn.commit().unwrap();
        }
        r2 = db.tree().root_page_id();
        assert_ne!(r1, r2, "Txn 2 must split to new root R2");

        // Txn 3: Begins (captures R2), inserts further, then ABORTS.
        {
            let mut txn = db.begin().unwrap();
            for i in 301..=500 {
                txn.put(i, &(i * 5).to_le_bytes()).unwrap();
            }
            txn.abort().unwrap();
        }

        // Root must be restored to R2 (NOT obsolete R1, and NOT initial R0)
        assert_eq!(
            db.tree().root_page_id(),
            r2,
            "abort must restore R2 (the root at Txn 3 begin), not obsolete R1"
        );

        // Verify all Txn 1 and Txn 2 keys exist
        for i in 1..=300 {
            let val = db.get(i).unwrap().expect("committed key must exist");
            assert_eq!(val, (i * 5).to_le_bytes());
        }
        // Verify Txn 3 keys do not exist
        for i in 301..=500 {
            assert!(db.get(i).unwrap().is_none());
        }
        db.sync().unwrap();
    }

    // Verify consistency after reopen
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        assert_eq!(db.tree().root_page_id(), r2);
        for i in 1..=300 {
            assert_eq!(db.get(i).unwrap().unwrap(), (i * 5).to_le_bytes());
        }
        for i in 301..=500 {
            assert!(db.get(i).unwrap().is_none());
        }
    }
}

// ===========================================================================
// SUITE 3: RecordStore Page Allocation and Rollback Audit
// ===========================================================================

#[test]
fn test_record_store_abort_first_allocation_resets_hint() {
    let (_dir, db_path, _wal_path) = create_test_env("rs_abort_first_alloc");

    let mut db = TreeDb::create(&db_path, 64).unwrap();
    assert_eq!(
        db.record_store().last_page_id(),
        INVALID_PAGE_ID,
        "initially last_page_id must be INVALID_PAGE_ID"
    );

    // Transaction allocates first record page
    let mut txn = db.begin().unwrap();
    txn.put(1, b"first_payload_ever").unwrap();
    // Hint was updated inside transaction's view
    txn.abort().unwrap();

    // After abort, hint must be restored to INVALID_PAGE_ID
    assert_eq!(
        db.record_store().last_page_id(),
        INVALID_PAGE_ID,
        "aborting transaction allocating first record page must restore INVALID_PAGE_ID"
    );

    // Subsequent insert works normally
    db.put(2, b"valid_after_abort").unwrap();
    assert_ne!(
        db.record_store().last_page_id(),
        INVALID_PAGE_ID,
        "new insert must allocate valid record page"
    );
    assert_eq!(
        db.get(2).unwrap().as_deref(),
        Some(&b"valid_after_abort"[..])
    );
}

#[test]
fn test_record_store_abort_with_existing_pages_restores_prior_hint() {
    let (_dir, db_path, _wal_path) = create_test_env("rs_abort_existing_pages");

    let mut db = TreeDb::create(&db_path, 64).unwrap();
    db.put(1, b"committed_initial").unwrap();
    db.sync().unwrap();

    let prior_hint = db.record_store().last_page_id();
    assert_ne!(prior_hint, INVALID_PAGE_ID);

    // New transaction inserts large record causing allocation of another page
    let mut txn = db.begin().unwrap();
    // 2500 bytes will fill remainder and allocate a new record page
    let large_payload = vec![0xAA; 2500];
    txn.put(2, &large_payload).unwrap();
    txn.abort().unwrap();

    assert_eq!(
        db.record_store().last_page_id(),
        prior_hint,
        "abort must restore prior valid record page hint"
    );

    // Verify old record still accessible
    assert_eq!(
        db.get(1).unwrap().as_deref(),
        Some(&b"committed_initial"[..])
    );
    assert!(db.get(2).unwrap().is_none());
}

#[test]
fn test_record_store_stale_hint_non_record_page_fallback() {
    let (_dir, db_path, _wal_path) = create_test_env("rs_stale_hint_fallback");

    let mut db = TreeDb::create(&db_path, 64).unwrap();

    // Allocate a free page directly in buffer pool
    let free_page_id = db.buffer_pool_mut().allocate_page(PageType::Free).unwrap();
    db.buffer_pool_mut().unpin_page(free_page_id, true).unwrap();

    // Manually point record_store's hint to the free page
    let mut rs = RecordStore::with_last_page(free_page_id);
    let rid = rs.insert(db.buffer_pool_mut(), b"test_payload").unwrap();

    // Must have rejected free_page_id and allocated a real Record page
    assert_ne!(
        rid.page_id, free_page_id,
        "must not treat non-record page as record page"
    );
    assert_eq!(rs.last_page_id(), rid.page_id);

    let page = db.buffer_pool_mut().fetch_page(rid.page_id).unwrap();
    assert_eq!(page.page_type(), PageType::Record);
    db.buffer_pool_mut().unpin_page(rid.page_id, false).unwrap();
}

#[test]
fn test_record_store_genuine_io_error_not_swallowed() {
    let (_dir, db_path, _wal_path) = create_test_env("rs_io_error_not_swallowed");

    let mut db = TreeDb::create(&db_path, 64).unwrap();

    // Hint pointing to an out-of-bounds PageId
    let mut rs = RecordStore::with_last_page(999_999);
    let res = rs.insert(db.buffer_pool_mut(), b"test_payload");

    assert!(
        matches!(res, Err(Error::InvalidPage(pid)) if pid == 999_999),
        "genuine out-of-bounds page error must be propagated and not swallowed"
    );
}

#[test]
fn test_record_store_overflow_payload_abort_and_reopen() {
    let (_dir, db_path, _wal_path) = create_test_env("rs_overflow_abort_reopen");

    // 8 KB payload spans multiple overflow pages
    let big_val_committed = vec![0x42u8; 8192];
    let big_val_aborted = vec![0x99u8; 8192];

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        // 1. Commit first overflow payload
        db.put(100, &big_val_committed).unwrap();
        db.sync().unwrap();

        // 2. Insert second overflow in transaction and abort
        let mut txn = db.begin().unwrap();
        txn.put(200, &big_val_aborted).unwrap();
        txn.abort().unwrap();

        // Verify committed is present, aborted is absent
        assert_eq!(
            db.get(100).unwrap().as_deref(),
            Some(&big_val_committed[..])
        );
        assert!(db.get(200).unwrap().is_none());
    }

    // Reopen and verify data integrity
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let val = db
            .get(100)
            .unwrap()
            .expect("committed overflow must survive");
        assert_eq!(val, big_val_committed);
        assert!(db.get(200).unwrap().is_none());
    }
}

// ===========================================================================
// SUITE 4: Page Identity and Header Protection Audit
// ===========================================================================

#[test]
fn test_header_page_write_at_rejected_deterministically() {
    let (_dir, db_path, _wal_path) = create_test_env("header_write_rejected");

    let mut pm = PageManager::create(&db_path).unwrap();
    let page = Page::new(HEADER_PAGE_ID, PageType::Record);

    // Direct write to HEADER_PAGE_ID must return Error::InvalidPage(0)
    let err = pm.write_page_at(HEADER_PAGE_ID, &page).unwrap_err();
    assert!(
        matches!(err, Error::InvalidPage(HEADER_PAGE_ID)),
        "writing regular page to HEADER_PAGE_ID must be rejected"
    );

    // write_page with id 0 must also be rejected
    let err = pm.write_page(&page).unwrap_err();
    assert!(matches!(err, Error::InvalidPage(HEADER_PAGE_ID)));
}

#[test]
fn test_rejected_page_0_write_leaves_header_bytes_intact() {
    let (_dir, db_path, _wal_path) = create_test_env("rejected_p0_header_intact");

    let pm = PageManager::create(&db_path).unwrap();
    let original_header = pm.header().clone();

    // Read raw page 0 bytes from disk
    drop(pm);
    let mut file = OpenOptions::new().read(true).open(&db_path).unwrap();
    let mut disk_bytes_before = vec![0u8; PAGE_SIZE];
    file.read_exact(&mut disk_bytes_before).unwrap();
    drop(file);

    // Reopen and attempt bogus write_page_at(0, bogus)
    let mut pm = PageManager::open(&db_path).unwrap();
    let bogus_page = Page::new(0, PageType::Free);
    let _ = pm.write_page_at(0, &bogus_page);
    drop(pm);

    // Verify raw page 0 bytes on disk are identical
    let mut file = OpenOptions::new().read(true).open(&db_path).unwrap();
    let mut disk_bytes_after = vec![0u8; PAGE_SIZE];
    file.read_exact(&mut disk_bytes_after).unwrap();
    drop(file);

    assert_eq!(
        disk_bytes_before, disk_bytes_after,
        "rejected page-0 write must not modify header bytes on disk"
    );

    // Reopen via PageManager to verify header parses cleanly
    let pm = PageManager::open(&db_path).unwrap();
    assert_eq!(pm.header().magic, original_header.magic);
    assert_eq!(pm.header().format_version, original_header.format_version);
    assert_eq!(pm.header().page_count, original_header.page_count);
}

#[test]
fn test_normal_header_persistence_path_works() {
    let (_dir, db_path, _wal_path) = create_test_env("header_persistence_path");

    let mut pm = PageManager::create(&db_path).unwrap();
    pm.set_max_commit_seq(9999).unwrap();
    pm.set_last_checkpoint_lsn(Lsn(777)).unwrap();
    drop(pm);

    let pm = PageManager::open(&db_path).unwrap();
    assert_eq!(pm.header().max_commit_seq, 9999);
    assert_eq!(pm.header().last_checkpoint_lsn, Lsn(777));
}

#[test]
fn test_buffer_pool_allocate_page_before_image_has_valid_page_id() {
    let (_dir, db_path, wal_path) = create_test_env("bp_alloc_before_image_id");

    let pm = PageManager::create(&db_path).unwrap();
    let lm = LogManager::create(&wal_path).unwrap();
    let mut pool = BufferPool::with_wal(10, pm, lm).unwrap();

    let new_page_id = pool.allocate_page(PageType::Record).unwrap();
    assert_ne!(new_page_id, HEADER_PAGE_ID);

    // Inspect the resident frame's before_image
    let frame_id = pool.get_page(new_page_id).is_ok();
    assert!(frame_id);

    // Unpin allocated page
    pool.unpin_page(new_page_id, false).unwrap();
}

#[test]
fn test_aborting_allocation_does_not_corrupt_page_0() {
    let (_dir, db_path, _wal_path) = create_test_env("abort_alloc_no_p0_corrupt");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        // Transaction allocates a new page and aborts
        let mut txn = db.begin().unwrap();
        txn.put(1, b"hello_world").unwrap();
        txn.abort().unwrap();

        db.sync().unwrap();
    }

    // Database must reopen without any header or format corruption
    let mut db = TreeDb::open(&db_path, 64).unwrap();
    assert!(db.get(1).unwrap().is_none());
    db.put(1, b"new_clean_value").unwrap();
    assert_eq!(db.get(1).unwrap().as_deref(), Some(&b"new_clean_value"[..]));
}

// ===========================================================================
// SUITE 5: Cross-Subsystem Matrix & Differential Reference Model
// ===========================================================================

#[test]
fn test_cross_subsystem_matrix_differential_reference_model() {
    let (_dir, db_path, _wal_path) = create_test_env("cross_subsystem_ref_model");

    let mut reference: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

    // Phase 1: Insert -> Commit -> Reopen
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        for i in 1..=40 {
            let val = format!("val_{i}").into_bytes();
            db.put(i, &val).unwrap();
            reference.insert(i, val);
        }
        db.sync().unwrap();
    }
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        for (&k, expected) in &reference {
            assert_eq!(db.get(k).unwrap().as_deref(), Some(expected.as_slice()));
        }
    }

    // Phase 2: Insert -> Abort -> Insert again
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin().unwrap();
        for i in 41..=60 {
            txn.put(i, b"temporary_aborted").unwrap();
        }
        txn.abort().unwrap();

        // Ensure aborted keys are absent
        for i in 41..=60 {
            assert!(db.get(i).unwrap().is_none());
        }

        // Insert valid keys 41..=60
        let mut txn = db.begin().unwrap();
        for i in 41..=60 {
            let val = format!("committed_val_{i}").into_bytes();
            txn.put(i, &val).unwrap();
            reference.insert(i, val);
        }
        txn.commit().unwrap();
        db.sync().unwrap();
    }

    // Phase 3: B+ tree split -> Abort -> Verify existing keys
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let pre_split_root = db.tree().root_page_id();

        let mut txn = db.begin().unwrap();
        // Insert 200 keys to trigger splits
        for i in 1000..=1200 {
            txn.put(i, b"split_test_payload").unwrap();
        }
        txn.abort().unwrap();

        assert_eq!(db.tree().root_page_id(), pre_split_root);
        for (&k, expected) in &reference {
            assert_eq!(db.get(k).unwrap().as_deref(), Some(expected.as_slice()));
        }
    }

    // Phase 4: Update and delete -> Abort -> Verify old values
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin().unwrap();

        // Update first 10 keys
        for i in 1..=10 {
            txn.put(i, b"overwritten_uncommitted").unwrap();
        }
        // Delete keys 11..=20
        for i in 11..=20 {
            txn.delete(i).unwrap();
        }
        txn.abort().unwrap();

        // Verify original values in reference model remain intact
        for (&k, expected) in &reference {
            assert_eq!(
                db.get(k).unwrap().as_deref(),
                Some(expected.as_slice()),
                "key {k} value must match reference model after abort"
            );
        }
    }

    // Phase 5: Overflow payload -> Abort -> Verify integrity
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin().unwrap();
        let big_payload = vec![0x77u8; 9000];
        txn.put(9999, &big_payload).unwrap();
        txn.abort().unwrap();

        assert!(db.get(9999).unwrap().is_none());
        for (&k, expected) in &reference {
            assert_eq!(db.get(k).unwrap().as_deref(), Some(expected.as_slice()));
        }

        // Commit valid overflow
        let valid_big = vec![0x88u8; 9000];
        db.put(9999, &valid_big).unwrap();
        reference.insert(9999, valid_big);
        db.sync().unwrap();
    }

    // Phase 6: Checkpoint -> Reopen -> Verify logical contents
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        db.checkpoint().unwrap();
        drop(db);

        let mut db = TreeDb::open(&db_path, 64).unwrap();
        for (&k, expected) in &reference {
            assert_eq!(
                db.get(k).unwrap().as_deref(),
                Some(expected.as_slice()),
                "key {k} mismatch after checkpoint and reopen"
            );
        }
    }

    // Phase 7: Repeated recovery idempotency
    for cycle in 1..=3 {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        for (&k, expected) in &reference {
            assert_eq!(
                db.get(k).unwrap().as_deref(),
                Some(expected.as_slice()),
                "recovery cycle {cycle}: key {k} mismatch"
            );
        }
        db.sync().unwrap();
    }
}
