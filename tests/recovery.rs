use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use tempfile::tempdir;
use treedb::{
    BufferPool, LeafNode, LogManager, LogRecordType, Lsn, Page, PageManager, PageType, RecordId,
    RecordPage, RecoveryEngine, RecoveryFault, TreeDb, NULL_LSN, PAGE_HEADER_SIZE, PAGE_SIZE,
};

fn create_test_db(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

#[test]
fn test_wal_header_and_initialization() {
    let (_dir, _db_path, wal_path) = create_test_db("header_test");
    let manager = LogManager::create(&wal_path).expect("create wal");
    assert_eq!(manager.flushed_lsn(), NULL_LSN);
    assert_eq!(manager.next_lsn(), Lsn(1));
    drop(manager);

    let mut reopened = LogManager::open(&wal_path).expect("open wal");
    assert_eq!(reopened.flushed_lsn(), NULL_LSN);
    assert_eq!(reopened.next_lsn(), Lsn(1));
    let records = reopened.read_all_records().expect("read records");
    assert!(records.is_empty());
}

#[test]
fn test_recovery_committed_insert() {
    let (_dir, db_path, wal_path) = create_test_db("commit_insert");

    // 1. Initialize DB and WAL
    let mut pm = PageManager::create(&db_path).unwrap();
    let page_id = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    // 2. Transaction inserts data on page_id in payload area
    let t1 = wal.begin_txn().unwrap();
    let offset = PAGE_HEADER_SIZE as u16;
    let before = vec![0u8; 16];
    let after = b"COMMITTED_DATA!!".to_vec();

    let lsn1 = wal
        .append_page_delta(t1, page_id, offset, before, after.clone())
        .unwrap();
    let _lsn_commit = wal.commit_txn(t1).unwrap();

    // Simulate crash before page was written to disk! (Page on disk has zeroes)
    drop(pm);
    drop(wal);

    // 3. Recover
    let mut pm_recover = PageManager::open(&db_path).unwrap();
    let mut wal_recover = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_recover, &mut wal_recover).unwrap();
    assert_eq!(stats.records_scanned, 3);
    assert_eq!(stats.committed_txns, 1);
    assert_eq!(stats.loser_txns, 0);
    assert_eq!(stats.redo_count, 1);
    assert_eq!(stats.undo_count, 0);

    // 4. Verify page was redone with committed data
    let recovered_page = pm_recover.read_page(page_id).unwrap();
    assert_eq!(
        &recovered_page.as_bytes()[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 16],
        &after[..]
    );
    assert_eq!(recovered_page.lsn(), lsn1);
}

#[test]
fn test_recovery_uncommitted_insert_undo() {
    let (_dir, db_path, wal_path) = create_test_db("uncommit_insert");

    // 1. Initialize DB and WAL
    let mut pm = PageManager::create(&db_path).unwrap();
    let page_id = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    // 2. Transaction modifies page and flushes page to disk before commit
    let t1 = wal.begin_txn().unwrap();
    let offset = PAGE_HEADER_SIZE as u16;
    let before = vec![0x11u8; 8];
    let after = vec![0x99u8; 8];

    let lsn1 = wal
        .append_page_delta(t1, page_id, offset, before.clone(), after.clone())
        .unwrap();
    wal.flush(lsn1).unwrap();

    // Write modified dirty page to disk (simulating buffer pool eviction before crash)
    let mut page = pm.read_page(page_id).unwrap();
    page.payload_mut()[0..8].copy_from_slice(&after);
    page.set_lsn(lsn1);
    pm.write_page(&page).unwrap();
    pm.sync().unwrap();

    // CRASH! (t1 was never committed)
    drop(pm);
    drop(wal);

    // 3. Recover
    let mut pm_recover = PageManager::open(&db_path).unwrap();
    let mut wal_recover = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_recover, &mut wal_recover).unwrap();
    assert_eq!(stats.records_scanned, 2);
    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 1);
    assert_eq!(stats.clrs_generated, 1);

    // 4. Verify uncommitted change was undone back to before-image
    let recovered_page = pm_recover.read_page(page_id).unwrap();
    assert_eq!(&recovered_page.payload()[0..8], &before[..]);
}

#[test]
fn test_recovery_committed_update() {
    let (_dir, db_path, wal_path) = create_test_db("commit_update");

    let mut pm = PageManager::create(&db_path).unwrap();
    let page_id = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let offset = (PAGE_HEADER_SIZE + 10) as u16;
    let old_val = b"OLD_VALUE".to_vec();
    let new_val = b"NEW_VALUE".to_vec();

    let lsn_delta = wal
        .append_page_delta(t1, page_id, offset, old_val, new_val.clone())
        .unwrap();
    wal.commit_txn(t1).unwrap();

    drop(pm);
    drop(wal);

    let mut pm_recover = PageManager::open(&db_path).unwrap();
    let mut wal_recover = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_recover, &mut wal_recover).unwrap();
    assert_eq!(stats.committed_txns, 1);
    assert_eq!(stats.redo_count, 1);

    let page = pm_recover.read_page(page_id).unwrap();
    assert_eq!(&page.payload()[10..10 + new_val.len()], &new_val[..]);
    assert_eq!(page.lsn(), lsn_delta);
}

#[test]
fn test_recovery_uncommitted_update_undo() {
    let (_dir, db_path, wal_path) = create_test_db("uncommit_update");

    let mut pm = PageManager::create(&db_path).unwrap();
    let page_id = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let offset = (PAGE_HEADER_SIZE + 20) as u16;
    let old_val = b"ORIGINAL_".to_vec();
    let dirty_val = b"CORRUPTED".to_vec();

    let lsn_delta = wal
        .append_page_delta(t1, page_id, offset, old_val.clone(), dirty_val.clone())
        .unwrap();
    wal.flush(lsn_delta).unwrap();

    // Dirty page reached disk before crash
    let mut page = pm.read_page(page_id).unwrap();
    page.payload_mut()[20..20 + dirty_val.len()].copy_from_slice(&dirty_val);
    page.set_lsn(lsn_delta);
    pm.write_page(&page).unwrap();
    pm.sync().unwrap();

    drop(pm);
    drop(wal);

    let mut pm_recover = PageManager::open(&db_path).unwrap();
    let mut wal_recover = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_recover, &mut wal_recover).unwrap();
    assert_eq!(stats.loser_txns, 1);
    assert_eq!(stats.undo_count, 1);
    assert_eq!(stats.clrs_generated, 1);

    let recovered_page = pm_recover.read_page(page_id).unwrap();
    assert_eq!(
        &recovered_page.payload()[20..20 + old_val.len()],
        &old_val[..]
    );
}

#[test]
fn test_recovery_interleaved_multi_transaction() {
    let (_dir, db_path, wal_path) = create_test_db("interleaved");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let p2 = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let offset0 = PAGE_HEADER_SIZE as u16;
    let offset10 = (PAGE_HEADER_SIZE + 10) as u16;

    // T1 begins, modifies p1
    let t1 = wal.begin_txn().unwrap();
    let _lsn1 = wal
        .append_page_delta(t1, p1, offset0, vec![0; 4], b"T1AA".to_vec())
        .unwrap();

    // T2 begins, modifies p2
    let t2 = wal.begin_txn().unwrap();
    let lsn2 = wal
        .append_page_delta(t2, p2, offset0, vec![0; 4], b"T2BB".to_vec())
        .unwrap();

    // T3 begins, modifies p1 at offset 10
    let t3 = wal.begin_txn().unwrap();
    let _lsn3 = wal
        .append_page_delta(t3, p1, offset10, vec![0; 4], b"T3CC".to_vec())
        .unwrap();

    // T1 commits
    wal.commit_txn(t1).unwrap();

    // T3 commits
    wal.commit_txn(t3).unwrap();

    // T2 is active (loser)! Flush T2's change to page p2 on disk
    let mut page2 = pm.read_page(p2).unwrap();
    page2.payload_mut()[0..4].copy_from_slice(b"T2BB");
    page2.set_lsn(lsn2);
    pm.write_page(&page2).unwrap();
    pm.sync().unwrap();

    drop(pm);
    drop(wal);

    // Recover
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut wal_rec = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
    assert_eq!(stats.committed_txns, 2);
    assert_eq!(stats.loser_txns, 1); // T2 is loser
    assert_eq!(stats.undo_count, 1); // T2 undone

    // Verify T1 and T3 changes on p1 are present
    let page1 = pm_rec.read_page(p1).unwrap();
    assert_eq!(&page1.payload()[0..4], b"T1AA");
    assert_eq!(&page1.payload()[10..14], b"T3CC");

    // Verify T2 changes on p2 were rolled back to zeroes
    let page2_rec = pm_rec.read_page(p2).unwrap();
    assert_eq!(&page2_rec.payload()[0..4], &[0, 0, 0, 0]);
}

#[test]
fn test_recovery_idempotence() {
    let (_dir, db_path, wal_path) = create_test_db("idempotence");

    let mut pm = PageManager::create(&db_path).unwrap();
    let page_id = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let offset0 = PAGE_HEADER_SIZE as u16;
    let offset8 = (PAGE_HEADER_SIZE + 8) as u16;

    let t1 = wal.begin_txn().unwrap();
    wal.append_page_delta(t1, page_id, offset0, vec![0; 4], b"IDEM".to_vec())
        .unwrap();
    wal.commit_txn(t1).unwrap();

    let t2 = wal.begin_txn().unwrap(); // loser
    let lsn_t2 = wal
        .append_page_delta(t2, page_id, offset8, vec![0; 4], b"LOSE".to_vec())
        .unwrap();
    wal.flush(lsn_t2).unwrap();

    let mut dirty_page = pm.read_page(page_id).unwrap();
    dirty_page.payload_mut()[0..4].copy_from_slice(b"IDEM");
    dirty_page.payload_mut()[8..12].copy_from_slice(b"LOSE");
    dirty_page.set_lsn(lsn_t2);
    pm.write_page(&dirty_page).unwrap();
    pm.sync().unwrap();

    drop(pm);
    drop(wal);

    // Pass 1 Recovery
    let mut pm1 = PageManager::open(&db_path).unwrap();
    let mut wal1 = LogManager::open(&wal_path).unwrap();
    let stats1 = RecoveryEngine::recover(&mut pm1, &mut wal1).unwrap();
    assert_eq!(stats1.loser_txns, 1);
    assert_eq!(stats1.undo_count, 1);

    let page_after_pass1 = pm1.read_page(page_id).unwrap();
    assert_eq!(&page_after_pass1.payload()[0..4], b"IDEM");
    assert_eq!(&page_after_pass1.payload()[8..12], &[0, 0, 0, 0]);
    drop(pm1);
    drop(wal1);

    // Pass 2 Recovery (Idempotence check)
    let mut pm2 = PageManager::open(&db_path).unwrap();
    let mut wal2 = LogManager::open(&wal_path).unwrap();
    let stats2 = RecoveryEngine::recover(&mut pm2, &mut wal2).unwrap();
    assert_eq!(stats2.loser_txns, 0); // No active losers remain!
    assert_eq!(stats2.undo_count, 0);

    let page_after_pass2 = pm2.read_page(page_id).unwrap();
    assert_eq!(&page_after_pass2.payload()[0..4], b"IDEM");
    assert_eq!(&page_after_pass2.payload()[8..12], &[0, 0, 0, 0]);
}

#[test]
fn test_wal_truncation_and_corruption_handling() {
    let (_dir, _db_path, wal_path) = create_test_db("truncation");

    let mut wal = LogManager::create(&wal_path).unwrap();
    let t1 = wal.begin_txn().unwrap();
    wal.append_page_delta(t1, 1, PAGE_HEADER_SIZE as u16, vec![1], vec![2])
        .unwrap();
    wal.commit_txn(t1).unwrap();

    // Now write a partial / truncated log record directly to WAL file
    drop(wal);
    let mut file = OpenOptions::new().append(true).open(&wal_path).unwrap();
    file.write_all(&[0x50, 0x00, 0x00, 0x00, 0x99, 0x88])
        .unwrap(); // Corrupted trailing fragment
    drop(file);

    // Open WAL: scanner should cleanly parse all valid records and stop gracefully at corrupt tail
    let mut reopened = LogManager::open(&wal_path).unwrap();
    let records = reopened.read_all_records().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].record_type, LogRecordType::Begin);
    assert_eq!(records[1].record_type, LogRecordType::PageDelta);
    assert_eq!(records[2].record_type, LogRecordType::Commit);
}

#[test]
fn test_write_ahead_rule_in_buffer_pool() {
    let (_dir, db_path, wal_path) = create_test_db("write_ahead");

    let pm = PageManager::create(&db_path).unwrap();
    let wal = LogManager::create(&wal_path).unwrap();
    let mut pool = BufferPool::with_wal(5, pm, wal).unwrap();

    let page_id = pool.allocate_page(PageType::Leaf).unwrap();
    let t1 = pool.wal_mut().unwrap().begin_txn().unwrap();

    let lsn = pool
        .wal_mut()
        .unwrap()
        .append_page_delta(
            t1,
            page_id,
            PAGE_HEADER_SIZE as u16,
            vec![0; 4],
            b"WAL1".to_vec(),
        )
        .unwrap();

    // Page has LSN = lsn in memory
    {
        let page = pool.get_page_mut(page_id).unwrap();
        page.set_lsn(lsn);
    }

    // Flush page to disk: BufferPool must automatically flush WAL up to `lsn` before writing page
    pool.flush_page(page_id).unwrap();

    // Verify WAL flushed LSN reached `lsn`
    assert!(pool.wal().unwrap().flushed_lsn() >= lsn);
}

#[test]
fn test_treedb_crash_recovery_integration() {
    let (_dir, db_path, _wal_path) = create_test_db("treedb_crash");

    // 1. Create TreeDb, insert keys
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(100, b"AliceInWonderland").unwrap();
        db.put(200, b"BobTheBuilder").unwrap();
        db.put(300, b"CharlieChaplin").unwrap();
        db.sync().unwrap();
    }

    // 2. Reopen TreeDb, verify recovery succeeds and data is accessible
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), Some(b"AliceInWonderland".to_vec()));
        assert_eq!(db.get(200).unwrap(), Some(b"BobTheBuilder".to_vec()));
        assert_eq!(db.get(300).unwrap(), Some(b"CharlieChaplin".to_vec()));

        // Delete a key
        db.delete(200).unwrap();
        assert_eq!(db.get(200).unwrap(), None);
        db.sync().unwrap();
    }

    // 3. Reopen again, verify deleted key remains absent and other keys present
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), Some(b"AliceInWonderland".to_vec()));
        assert_eq!(db.get(200).unwrap(), None);
        assert_eq!(db.get(300).unwrap(), Some(b"CharlieChaplin".to_vec()));
    }
}

#[test]
fn test_bplus_tree_structural_recovery() {
    let (_dir, db_path, wal_path) = create_test_db("btree_recovery");

    let mut pm = PageManager::create(&db_path).unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let root_id = pm.allocate_page().unwrap();
    let mut leaf = LeafNode::new();
    for i in 1..=50 {
        leaf.insert(i as u64, RecordId::new(10, i as u32)).unwrap();
    }

    let mut page = Page::new(root_id, PageType::Leaf);
    leaf.serialize(&mut page).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let _lsn = wal
        .append_page_delta(t1, root_id, 0, vec![0; PAGE_SIZE], page.as_bytes().to_vec())
        .unwrap();
    wal.commit_txn(t1).unwrap();

    drop(pm);
    drop(wal);

    // Recover
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut wal_rec = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
    assert_eq!(stats.redo_count, 1);

    // Verify leaf node is deserializable and valid
    let rec_page = pm_rec.read_page(root_id).unwrap();
    let restored_leaf = LeafNode::deserialize(&rec_page).unwrap();
    assert_eq!(restored_leaf.key_count(), 50);
    assert_eq!(restored_leaf.get(25), Some(RecordId::new(10, 25)));
}

#[test]
fn test_record_store_structural_recovery() {
    let (_dir, db_path, wal_path) = create_test_db("record_store_recovery");

    let mut pm = PageManager::create(&db_path).unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let page_id = pm.allocate_page().unwrap();
    let mut rp = RecordPage::new();
    let s0 = rp.insert(b"FirstRecordPayload").unwrap();
    let s1 = rp.insert(b"SecondRecordPayload").unwrap();

    let mut page = Page::new(page_id, PageType::Record);
    rp.serialize(&mut page).unwrap();

    let t1 = wal.begin_txn().unwrap();
    wal.append_page_delta(t1, page_id, 0, vec![0; PAGE_SIZE], page.as_bytes().to_vec())
        .unwrap();
    wal.commit_txn(t1).unwrap();

    drop(pm);
    drop(wal);

    // Recover
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut wal_rec = LogManager::open(&wal_path).unwrap();

    let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
    assert_eq!(stats.redo_count, 1);

    let rec_page = pm_rec.read_page(page_id).unwrap();
    let restored_rp = RecordPage::deserialize(&rec_page).unwrap();
    assert_eq!(restored_rp.get(s0), Some(&b"FirstRecordPayload"[..]));
    assert_eq!(restored_rp.get(s1), Some(&b"SecondRecordPayload"[..]));
}

#[test]
fn test_wal_ordering_on_lru_eviction() {
    let (_dir, db_path, wal_path) = create_test_db("lru_wal_order");

    let pm = PageManager::create(&db_path).unwrap();
    let wal = LogManager::create(&wal_path).unwrap();
    // Capacity 2 frames
    let mut pool = BufferPool::with_wal(2, pm, wal).unwrap();

    let p1 = pool.allocate_page(PageType::Leaf).unwrap();
    pool.unpin_page(p1, false).unwrap();
    let p2 = pool.allocate_page(PageType::Leaf).unwrap();
    pool.unpin_page(p2, false).unwrap();

    // Start transaction and modify p1
    let t1 = pool.wal_mut().unwrap().begin_txn().unwrap();
    pool.set_active_txn(t1);

    {
        let page1 = pool.fetch_page_mut(p1).unwrap();
        page1.payload_mut()[0..4].copy_from_slice(b"P1DD");
    }
    pool.unpin_page(p1, true).unwrap();

    // Access p2 so p1 is the older frame in LRU
    let _ = pool.fetch_page(p2).unwrap();
    pool.unpin_page(p2, false).unwrap();

    // Allocate p3 -> triggers LRU eviction of p1
    let p3 = pool.allocate_page(PageType::Leaf).unwrap();
    pool.unpin_page(p3, false).unwrap();

    // Verify p1 was evicted and flushed to disk, and WAL flushed_lsn >= p1 disk LSN
    let flushed_lsn = pool.wal().unwrap().flushed_lsn();
    assert!(flushed_lsn.is_valid());

    let mut pm_check = PageManager::open(&db_path).unwrap();
    let disk_p1 = pm_check.read_page(p1).unwrap();
    assert_eq!(&disk_p1.payload()[0..4], b"P1DD");
    assert!(disk_p1.lsn() <= flushed_lsn);
    assert_eq!(disk_p1.lsn(), flushed_lsn);
}

#[test]
fn test_clr_durability_and_restart_ordering() {
    let (_dir, db_path, wal_path) = create_test_db("clr_durability");

    // 1. Initialize DB with page 1 modified by uncommitted loser T1
    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let offset = PAGE_HEADER_SIZE as u16;
    let before = b"ORIGINAL".to_vec();
    let after = b"MUTATED_".to_vec();

    let lsn_delta = wal
        .append_page_delta(t1, p1, offset, before.clone(), after.clone())
        .unwrap();
    wal.flush(lsn_delta).unwrap();

    // Flush dirty page to disk
    let mut page1 = pm.read_page(p1).unwrap();
    page1.payload_mut()[0..8].copy_from_slice(&after);
    page1.set_lsn(lsn_delta);
    pm.write_page(&page1).unwrap();
    pm.sync().unwrap();

    drop(pm);
    drop(wal);

    // 2. Fault injection: crash immediately after CLR flush before page write
    {
        let mut pm_rec = PageManager::open(&db_path).unwrap();
        let mut wal_rec = LogManager::open(&wal_path).unwrap();
        let err = RecoveryEngine::recover_with_fault(
            &mut pm_rec,
            &mut wal_rec,
            Some(RecoveryFault::AfterClrFlushBeforePageWrite),
        )
        .unwrap_err();
        assert!(matches!(err, treedb::Error::RecoveryInterrupted(_)));
    }

    // 3. Restart recovery cleanly: Redo should replay the durable CLR and finish undo
    {
        let mut pm_rec = PageManager::open(&db_path).unwrap();
        let mut wal_rec = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
        assert!(stats.redo_count >= 1);

        let recovered_page = pm_rec.read_page(p1).unwrap();
        assert_eq!(&recovered_page.payload()[0..8], &before[..]);
    }
}

#[test]
fn test_crash_during_recovery_fault_matrix() {
    let faults = [
        RecoveryFault::BeforeAnalysis,
        RecoveryFault::BeforeRedo,
        RecoveryFault::DuringRedo(1),
        RecoveryFault::BeforeUndo,
        RecoveryFault::AfterClrFlushBeforePageWrite,
        RecoveryFault::AfterRecoveredPageWrite(1),
    ];

    for (idx, fault) in faults.iter().enumerate() {
        let (_dir, db_path, wal_path) = create_test_db(&format!("fault_matrix_{idx}"));

        let mut pm = PageManager::create(&db_path).unwrap();
        let p1 = pm.allocate_page().unwrap();
        let mut wal = LogManager::create(&wal_path).unwrap();

        // Committed T1
        let t1 = wal.begin_txn().unwrap();
        let _l1 = wal
            .append_page_delta(
                t1,
                p1,
                PAGE_HEADER_SIZE as u16,
                vec![0; 4],
                b"COMM".to_vec(),
            )
            .unwrap();
        wal.commit_txn(t1).unwrap();

        // Loser T2
        let t2 = wal.begin_txn().unwrap();
        let l2 = wal
            .append_page_delta(
                t2,
                p1,
                (PAGE_HEADER_SIZE + 10) as u16,
                vec![0; 4],
                b"LOSE".to_vec(),
            )
            .unwrap();
        wal.flush(l2).unwrap();

        // Leave page at LSN 0 on disk to ensure Redo executes
        let mut p = pm.read_page(p1).unwrap();
        p.payload_mut()[0..4].copy_from_slice(b"COMM");
        p.payload_mut()[10..14].copy_from_slice(b"LOSE");
        p.set_lsn(NULL_LSN);
        pm.write_page(&p).unwrap();
        pm.sync().unwrap();

        drop(pm);
        drop(wal);

        // 1. Inject fault
        {
            let mut pm_rec = PageManager::open(&db_path).unwrap();
            let mut wal_rec = LogManager::open(&wal_path).unwrap();
            let err = RecoveryEngine::recover_with_fault(&mut pm_rec, &mut wal_rec, Some(*fault))
                .unwrap_err();
            assert!(matches!(err, treedb::Error::RecoveryInterrupted(_)));
        }

        // 2. Restart recovery cleanly
        {
            let mut pm_rec = PageManager::open(&db_path).unwrap();
            let mut wal_rec = LogManager::open(&wal_path).unwrap();
            let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
            assert!(stats.committed_txns >= 1);

            let restored_page = pm_rec.read_page(p1).unwrap();
            assert_eq!(&restored_page.payload()[0..4], b"COMM");
            assert_eq!(&restored_page.payload()[10..14], &[0, 0, 0, 0]);
        }
    }
}

#[test]
fn test_treedb_transaction_committed_and_uncommitted_put() {
    let (_dir, db_path, _wal_path) = create_test_db("treedb_txn_put");

    // 1. Committed put
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(100, b"Val100_Committed").unwrap();
        db.put(200, b"Val200_Committed").unwrap();
        // Drop without explicit flush_all - WAL commit flushed log
    }

    // 2. Reopen: verify committed puts exist
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), Some(b"Val100_Committed".to_vec()));
        assert_eq!(db.get(200).unwrap(), Some(b"Val200_Committed".to_vec()));

        // 3. Uncommitted put: start transaction, insert key 300, flush frames to disk, crash
        let t = db.begin_txn().unwrap();
        db.put(300, b"Val300_Uncommitted").unwrap();
        // Force dirty pages to disk before crash (simulating eviction)
        db.buffer_pool_mut().flush_all().unwrap();
        assert!(db.buffer_pool().wal().unwrap().flushed_lsn().is_valid());
        // Do NOT commit! Crash.
        let _ = t;
    }

    // 4. Reopen: verify uncommitted key 300 was undone and does NOT exist
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), Some(b"Val100_Committed".to_vec()));
        assert_eq!(db.get(200).unwrap(), Some(b"Val200_Committed".to_vec()));
        assert_eq!(db.get(300).unwrap(), None);
    }
}

#[test]
fn test_treedb_transaction_committed_and_uncommitted_delete() {
    let (_dir, db_path, _wal_path) = create_test_db("treedb_txn_del");

    // Initialize with keys 100, 200, 300
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(100, b"Val100").unwrap();
        db.put(200, b"Val200").unwrap();
        db.put(300, b"Val300").unwrap();
    }

    // Committed delete of 100
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        let deleted = db.delete(100).unwrap();
        assert_eq!(deleted, Some(b"Val100".to_vec()));
    }

    // Verify 100 is absent, and perform uncommitted delete of 200
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), None);
        assert_eq!(db.get(200).unwrap(), Some(b"Val200".to_vec()));

        let _t = db.begin_txn().unwrap();
        let deleted = db.delete(200).unwrap();
        assert_eq!(deleted, Some(b"Val200".to_vec()));
        db.buffer_pool_mut().flush_all().unwrap();
        // Crash without commit
    }

    // Reopen: verify 200 was restored!
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(100).unwrap(), None);
        assert_eq!(db.get(200).unwrap(), Some(b"Val200".to_vec()));
        assert_eq!(db.get(300).unwrap(), Some(b"Val300".to_vec()));
    }
}

#[test]
fn test_treedb_transaction_committed_and_uncommitted_update() {
    let (_dir, db_path, _wal_path) = create_test_db("treedb_txn_upd");

    // Initialize key 500
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        db.put(500, b"OriginalVal").unwrap();
    }

    // Committed update to "UpdatedVal"
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        db.put(500, b"UpdatedVal").unwrap();
    }

    // Verify committed update, then perform uncommitted update to "DirtyVal"
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(500).unwrap(), Some(b"UpdatedVal".to_vec()));

        let _t = db.begin_txn().unwrap();
        db.put(500, b"DirtyVal").unwrap();
        db.buffer_pool_mut().flush_all().unwrap();
        // Crash without commit
    }

    // Reopen: verify 500 rolled back to "UpdatedVal"
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        assert_eq!(db.get(500).unwrap(), Some(b"UpdatedVal".to_vec()));
    }
}

#[test]
fn test_index_data_atomicity_comprehensive() {
    let (_dir, db_path, _wal_path) = create_test_db("atomicity_check");

    // 1. Insert 40 initial committed keys
    {
        let mut db = TreeDb::create(&db_path, 10).unwrap();
        for i in 1..=40 {
            let val = format!("CommittedPayload_{i:04}");
            db.put(i as u64, val.as_bytes()).unwrap();
        }
    }

    // 2. Start loser transaction with interleaved mutations:
    // - Insert 10 new keys (41..=50)
    // - Update 10 existing keys (1..=10)
    // - Delete 10 existing keys (11..=20)
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();
        let _t = db.begin_txn().unwrap();

        for i in 41..=50 {
            let val = format!("UncommittedNew_{i:04}");
            db.put(i as u64, val.as_bytes()).unwrap();
        }
        for i in 1..=10 {
            let val = format!("UncommittedUpd_{i:04}");
            db.put(i as u64, val.as_bytes()).unwrap();
        }
        for i in 11..=20 {
            db.delete(i as u64).unwrap();
        }

        // Flush all pages to disk to simulate intermediate eviction
        db.buffer_pool_mut().flush_all().unwrap();
        // Crash without commit
    }

    // 3. Reopen and verify index/data atomicity
    {
        let mut db = TreeDb::open(&db_path, 10).unwrap();

        // Check keys 1..=10 have original values
        for i in 1..=10 {
            let expected = format!("CommittedPayload_{i:04}").into_bytes();
            assert_eq!(db.get(i as u64).unwrap(), Some(expected));
        }

        // Check keys 11..=20 exist with original values
        for i in 11..=20 {
            let expected = format!("CommittedPayload_{i:04}").into_bytes();
            assert_eq!(db.get(i as u64).unwrap(), Some(expected));
        }

        // Check keys 21..=40 exist
        for i in 21..=40 {
            let expected = format!("CommittedPayload_{i:04}").into_bytes();
            assert_eq!(db.get(i as u64).unwrap(), Some(expected));
        }

        // Check keys 41..=50 do not exist
        for i in 41..=50 {
            assert_eq!(db.get(i as u64).unwrap(), None);
        }

        // Invariant: scan all entries and ensure every RecordId points to a valid record
        let scan_results = db.scan(None, None).unwrap();
        assert_eq!(scan_results.len(), 40);
        for (idx, (k, val)) in scan_results.into_iter().enumerate() {
            let expected_key = (idx + 1) as u64;
            assert_eq!(k, expected_key);
            let expected_val = format!("CommittedPayload_{expected_key:04}").into_bytes();
            assert_eq!(val, expected_val);
        }

        // Pin discipline
        for pid in 0..db.buffer_pool().page_count() {
            if db.buffer_pool().is_resident(pid) {
                assert_eq!(db.buffer_pool().pin_count(pid).unwrap(), 0);
            }
        }
    }
}

#[test]
fn test_transaction_identity_and_prev_lsn_chain() {
    let (_dir, _db_path, wal_path) = create_test_db("txn_identity");

    let mut wal = LogManager::create(&wal_path).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let l1_1 = wal.append_page_delta(t1, 1, 0, vec![0], vec![1]).unwrap();
    let l1_2 = wal.append_page_delta(t1, 1, 1, vec![0], vec![2]).unwrap();

    let t2 = wal.begin_txn().unwrap();
    let l2_1 = wal.append_page_delta(t2, 2, 0, vec![0], vec![9]).unwrap();

    let l1_3 = wal.append_page_delta(t1, 1, 2, vec![0], vec![3]).unwrap();
    let l1_c = wal.commit_txn(t1).unwrap();

    let l2_a = wal.abort_txn(t2).unwrap();

    drop(wal);

    let mut wal_read = LogManager::open(&wal_path).unwrap();
    let records = wal_read.read_all_records().unwrap();

    // Map records by LSN
    let by_lsn: std::collections::HashMap<Lsn, &treedb::LogRecord> =
        records.iter().map(|r| (r.lsn, r)).collect();

    // Verify T1 chain: l1_c -> l1_3 -> l1_2 -> l1_1 -> t1 (Begin) -> NULL_LSN
    assert_eq!(by_lsn[&l1_c].prev_lsn, l1_3);
    assert_eq!(by_lsn[&l1_3].prev_lsn, l1_2);
    assert_eq!(by_lsn[&l1_2].prev_lsn, l1_1);
    assert_eq!(by_lsn[&l1_1].prev_lsn, Lsn(1));
    assert_eq!(by_lsn[&Lsn(1)].prev_lsn, NULL_LSN);

    // Verify T2 chain: l2_a -> l2_1 -> t2 (Begin) -> NULL_LSN
    assert_eq!(by_lsn[&l2_a].prev_lsn, l2_1);
    assert_eq!(by_lsn[&l2_1].prev_lsn, Lsn(4));
    assert_eq!(by_lsn[&Lsn(4)].prev_lsn, NULL_LSN);
}

#[test]
fn test_wal_corruption_matrix() {
    let (_dir, _db_path, wal_path) = create_test_db("corruption_matrix");

    // 1. Empty WAL after header
    {
        let wal = LogManager::create(&wal_path).unwrap();
        drop(wal);
        let mut reopened = LogManager::open(&wal_path).unwrap();
        let records = reopened.read_all_records().unwrap();
        assert!(records.is_empty());
    }

    // 2. Truncated tail variations (un-flushed trailing garbage)
    let tail_garbage = [
        vec![0x01],                         // 1-byte truncated length
        vec![0x10, 0x00],                   // 2-byte truncated length
        vec![0x20, 0x00, 0x00, 0x00, 0x01], // truncated record header
    ];

    for (idx, garbage) in tail_garbage.iter().enumerate() {
        let (_dir, _db_path, wpath) = create_test_db(&format!("tail_garbage_{idx}"));
        let mut wal = LogManager::create(&wpath).unwrap();
        let t1 = wal.begin_txn().unwrap();
        wal.commit_txn(t1).unwrap();
        drop(wal);

        // Append trailing garbage
        let mut file = OpenOptions::new().append(true).open(&wpath).unwrap();
        file.write_all(garbage).unwrap();
        drop(file);

        let mut reopened = LogManager::open(&wpath).unwrap();
        let records = reopened.read_all_records().unwrap();
        assert_eq!(records.len(), 2);
    }

    // 3. Mid-log corruption before flushed_lsn -> returns WalCorruption on open
    {
        let (_dir, _db_path, wpath) = create_test_db("mid_log_corrupt");
        let mut wal = LogManager::create(&wpath).unwrap();
        let t1 = wal.begin_txn().unwrap();
        let lsn = wal.append_page_delta(t1, 1, 0, vec![1], vec![2]).unwrap();
        wal.commit_txn(t1).unwrap();
        wal.flush(lsn).unwrap();
        drop(wal);

        // Corrupt bytes inside the second record (page delta)
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&wpath)
            .unwrap();
        // Seek past WAL header (32 bytes) + first record (approx 20 bytes)
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(45)).unwrap();
        file.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        drop(file);

        let err = LogManager::open(&wpath).unwrap_err();
        assert!(matches!(err, treedb::Error::WalCorruption(_)));
    }
}

#[test]
fn test_pagelsn_semantics_matrix() {
    let (_dir, db_path, wal_path) = create_test_db("pagelsn_matrix");

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let p2 = pm.allocate_page().unwrap();
    let p3 = pm.allocate_page().unwrap();
    let mut wal = LogManager::create(&wal_path).unwrap();

    let t1 = wal.begin_txn().unwrap();
    let lsn1 = wal
        .append_page_delta(
            t1,
            p1,
            PAGE_HEADER_SIZE as u16,
            vec![0; 4],
            b"RED1".to_vec(),
        )
        .unwrap();
    let lsn2 = wal
        .append_page_delta(
            t1,
            p2,
            PAGE_HEADER_SIZE as u16,
            vec![0; 4],
            b"RED2".to_vec(),
        )
        .unwrap();
    let _lsn3 = wal
        .append_page_delta(
            t1,
            p3,
            PAGE_HEADER_SIZE as u16,
            vec![0; 4],
            b"RED3".to_vec(),
        )
        .unwrap();
    wal.commit_txn(t1).unwrap();

    // Set page states on disk:
    // p1: page_lsn == 0 (< lsn1) -> should be redone
    let mut page1 = pm.read_page(p1).unwrap();
    page1.set_lsn(NULL_LSN);
    pm.write_page(&page1).unwrap();

    // p2: page_lsn == lsn2 -> should be skipped
    let mut page2 = pm.read_page(p2).unwrap();
    page2.payload_mut()[0..4].copy_from_slice(b"RED2");
    page2.set_lsn(lsn2);
    pm.write_page(&page2).unwrap();

    // p3: page_lsn > lsn3 (simulating higher LSN from later operation) -> should be skipped
    let mut page3 = pm.read_page(p3).unwrap();
    page3.payload_mut()[0..4].copy_from_slice(b"NEW3");
    page3.set_lsn(Lsn(999));
    pm.write_page(&page3).unwrap();
    pm.sync().unwrap();

    drop(pm);
    drop(wal);

    // Recover
    let mut pm_rec = PageManager::open(&db_path).unwrap();
    let mut wal_rec = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm_rec, &mut wal_rec).unwrap();
    // Only p1 should be redone
    assert_eq!(stats.redo_count, 1);

    // Verify p1 received RED1 with PageLSN == lsn1
    let recovered_p1 = pm_rec.read_page(p1).unwrap();
    assert_eq!(&recovered_p1.payload()[0..4], b"RED1");
    assert_eq!(recovered_p1.lsn(), lsn1);

    // Verify p2 untouched
    let recovered_p2 = pm_rec.read_page(p2).unwrap();
    assert_eq!(&recovered_p2.payload()[0..4], b"RED2");
    assert_eq!(recovered_p2.lsn(), lsn2);

    // Verify p3 untouched (no LSN regression)
    let recovered_p3 = pm_rec.read_page(p3).unwrap();
    assert_eq!(&recovered_p3.payload()[0..4], b"NEW3");
    assert_eq!(recovered_p3.lsn(), Lsn(999));
}

#[test]
fn test_recovery_deep_idempotence_4_passes() {
    let (_dir, db_path, wal_path) = create_test_db("deep_idempotence");

    // 1. Setup DB with committed and uncommitted transactions
    let p1 = {
        let mut pm = PageManager::create(&db_path).unwrap();
        let p1 = pm.allocate_page().unwrap();
        let mut wal = LogManager::create(&wal_path).unwrap();

        let t1 = wal.begin_txn().unwrap();
        wal.append_page_delta(
            t1,
            p1,
            PAGE_HEADER_SIZE as u16,
            vec![0; 4],
            b"PASS".to_vec(),
        )
        .unwrap();
        wal.commit_txn(t1).unwrap();

        let t2 = wal.begin_txn().unwrap();
        let lsn_t2 = wal
            .append_page_delta(
                t2,
                p1,
                (PAGE_HEADER_SIZE + 10) as u16,
                vec![0; 4],
                b"DIRT".to_vec(),
            )
            .unwrap();
        wal.flush(lsn_t2).unwrap();

        let mut page = pm.read_page(p1).unwrap();
        page.payload_mut()[0..4].copy_from_slice(b"PASS");
        page.payload_mut()[10..14].copy_from_slice(b"DIRT");
        page.set_lsn(lsn_t2);
        pm.write_page(&page).unwrap();
        pm.sync().unwrap();
        p1
    };

    // Capture bytes across 4 passes
    let mut page_snapshots = Vec::new();

    for pass in 1..=4 {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut wal = LogManager::open(&wal_path).unwrap();

        let stats = RecoveryEngine::recover(&mut pm, &mut wal).unwrap();
        if pass > 1 {
            assert_eq!(stats.loser_txns, 0);
            assert_eq!(stats.undo_count, 0);
        }

        let p1_page = pm.read_page(p1).unwrap();
        assert_eq!(&p1_page.payload()[0..4], b"PASS");
        assert_eq!(&p1_page.payload()[10..14], &[0, 0, 0, 0]);

        page_snapshots.push(p1_page.as_bytes().to_vec());
    }

    // Passes 2, 3, and 4 must have bit-for-bit identical page bytes
    assert_eq!(page_snapshots[1], page_snapshots[2]);
    assert_eq!(page_snapshots[2], page_snapshots[3]);
}

#[test]
fn test_storage_format_v1_migration_rejection_and_v2_record_size() {
    let (_dir, db_path, _wal_path) = create_test_db("format_migration");

    // 1. Test that creating a v1 format file causes UnsupportedFormatVersion on open
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&db_path)
            .unwrap();

        let v1_header = treedb::DbHeader::new();
        // Overwrite format version to 1
        let mut header_bytes = v1_header.serialize();
        header_bytes[8..12].copy_from_slice(&1u32.to_le_bytes()); // format_version = 1
        file.write_all(&header_bytes).unwrap();

        let metadata_page = Page::new(1, PageType::Metadata);
        file.write_all(metadata_page.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let err = PageManager::open(&db_path).unwrap_err();
        assert!(matches!(
            err,
            treedb::Error::UnsupportedFormatVersion {
                found: 1,
                supported: 2
            }
        ));
    }

    // 2. Test that v2 format works and validates record size boundary (4070 vs 4071)
    let (_dir2, db_path2, _wal_path2) = create_test_db("record_size_bound");
    {
        let mut db = TreeDb::create(&db_path2, 10).unwrap();

        // 4070 bytes: MAX_RECORD_SIZE (4096 - 16 header - 6 rec header - 4 slot)
        let max_val = vec![0xAA; treedb::MAX_RECORD_SIZE];
        db.put(1, &max_val).unwrap();
        assert_eq!(db.get(1).unwrap(), Some(max_val));

        // 4071 bytes: MAX_RECORD_SIZE + 1
        let oversized = vec![0xBB; treedb::MAX_RECORD_SIZE + 1];
        let err = db.put(2, &oversized).unwrap_err();
        assert!(matches!(
            err,
            treedb::Error::ValueTooLarge {
                size: 4071,
                limit: 4070
            }
        ));
    }
}

#[test]
fn test_recovery_pin_discipline() {
    let (_dir, db_path, _wal_path) = create_test_db("pin_discipline");

    {
        let mut db = TreeDb::create(&db_path, 5).unwrap();
        for i in 1..=100 {
            db.put(i as u64, format!("PinValue_{i}").as_bytes())
                .unwrap();
        }

        // Verify all resident pages have pin count == 0
        for pid in 0..db.buffer_pool().page_count() {
            if db.buffer_pool().is_resident(pid) {
                assert_eq!(db.buffer_pool().pin_count(pid).unwrap(), 0);
            }
        }
    }

    {
        let mut db = TreeDb::open(&db_path, 5).unwrap();
        for i in 1..=100 {
            assert!(db.get(i as u64).unwrap().is_some());
        }

        for pid in 0..db.buffer_pool().page_count() {
            if db.buffer_pool().is_resident(pid) {
                assert_eq!(db.buffer_pool().pin_count(pid).unwrap(), 0);
            }
        }
    }
}
