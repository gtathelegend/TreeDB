use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use tempfile::tempdir;
use treedb::{
    BufferPool, LeafNode, LogManager, LogRecordType, Lsn, Page, PageManager, PageType, RecordId,
    RecordPage, RecoveryEngine, TreeDb, NULL_LSN, PAGE_HEADER_SIZE, PAGE_SIZE,
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
