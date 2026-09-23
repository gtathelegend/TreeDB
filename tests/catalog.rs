use std::collections::BTreeMap;
use treedb::engine::catalog::{
    TableId, TableMetadata, CATALOG_HEADER_SIZE, CATALOG_MAGIC, CURRENT_CATALOG_VERSION,
    MAX_TABLE_NAME_LEN,
};
use treedb::error::Error;
use treedb::storage::metadata::DEFAULT_METADATA_PAGE_ID;
use treedb::storage::page_manager::PageManager;
use treedb::TreeDb;

// ============================================================================
// Helper Utilities
// ============================================================================

fn temp_db_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("catalog_test.treedb");
    (dir, path)
}

// ============================================================================
// 1. Catalog Lifecycle Tests
// ============================================================================

#[test]
fn test_catalog_empty_database_and_initial_state() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    assert_eq!(db.list_tables().expect("list tables"), Vec::new());
    assert_eq!(db.get_table("users").expect("get table"), None);
    assert_eq!(
        db.get_table_by_id(TableId(1)).expect("get table by id"),
        None
    );

    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_create_and_lookup_table() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let table_id = db.create_table("users").expect("create table");
    assert_eq!(table_id, TableId(1));

    let meta = db
        .get_table("users")
        .expect("get table")
        .expect("table should exist");
    assert_eq!(meta.table_id, TableId(1));
    assert_eq!(meta.name, "users");
    assert_eq!(meta.schema_version, 1);
    assert!(meta.root_page_id > 1);
    assert!(meta.record_page_id > 1);
    assert_ne!(meta.root_page_id, meta.record_page_id);

    let meta_by_id = db
        .get_table_by_id(table_id)
        .expect("get by id")
        .expect("table should exist");
    assert_eq!(meta, meta_by_id);

    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_create_duplicate_rejected() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("orders").expect("create table orders");
    let err = db.create_table("orders").unwrap_err();
    assert!(
        matches!(err, Error::TableAlreadyExists(ref name) if name == "orders"),
        "expected TableAlreadyExists, got {err:?}"
    );

    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_invalid_names_rejected() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    // Empty name
    let err_empty = db.create_table("").unwrap_err();
    assert!(matches!(err_empty, Error::InvalidTableName(_)));

    // Name exceeding MAX_TABLE_NAME_LEN (64 bytes)
    let long_name = "a".repeat(MAX_TABLE_NAME_LEN + 1);
    let err_long = db.create_table(&long_name).unwrap_err();
    assert!(matches!(err_long, Error::InvalidTableName(_)));

    // Exactly MAX_TABLE_NAME_LEN is allowed
    let exact_name = "b".repeat(MAX_TABLE_NAME_LEN);
    let tid = db
        .create_table(&exact_name)
        .expect("exact length name allowed");
    assert_eq!(tid, TableId(1));

    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_list_tables() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let t1 = db.create_table("table_alpha").expect("create alpha");
    let t2 = db.create_table("table_beta").expect("create beta");
    let t3 = db.create_table("table_gamma").expect("create gamma");

    assert_eq!(t1, TableId(1));
    assert_eq!(t2, TableId(2));
    assert_eq!(t3, TableId(3));

    let tables = db.list_tables().expect("list tables");
    assert_eq!(tables.len(), 3);
    assert_eq!(tables[0].name, "table_alpha");
    assert_eq!(tables[1].name, "table_beta");
    assert_eq!(tables[2].name, "table_gamma");

    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_drop_table_and_lookup() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let t1 = db.create_table("users").expect("create users");
    let t2 = db.create_table("posts").expect("create posts");

    db.drop_table("users").expect("drop users");

    assert_eq!(db.get_table("users").expect("get users"), None);
    assert_eq!(db.get_table_by_id(t1).expect("get by id"), None);

    let posts = db
        .get_table("posts")
        .expect("get posts")
        .expect("posts should remain");
    assert_eq!(posts.table_id, t2);

    let tables = db.list_tables().expect("list tables");
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "posts");

    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

#[test]
fn test_catalog_drop_missing_rejected() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let err = db.drop_table("nonexistent").unwrap_err();
    assert!(
        matches!(err, Error::TableNotFound(ref name) if name == "nonexistent"),
        "expected TableNotFound, got {err:?}"
    );

    db.validate_catalog().expect("validate catalog");
}

// ============================================================================
// 2. Persistence & Stable TableId Tests
// ============================================================================

#[test]
fn test_catalog_persistence_across_close_reopen() {
    let (_dir, path) = temp_db_path();

    let (t1, t2) = {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        let id1 = db.create_table("customers").expect("create customers");
        let id2 = db.create_table("orders").expect("create orders");
        db.sync().expect("sync");
        (id1, id2)
    };

    // Reopen database
    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");

    let c = db
        .get_table("customers")
        .expect("get customers")
        .expect("customers exists");
    assert_eq!(c.table_id, t1);
    assert_eq!(c.name, "customers");

    let o = db
        .get_table("orders")
        .expect("get orders")
        .expect("orders exists");
    assert_eq!(o.table_id, t2);
    assert_eq!(o.name, "orders");

    let tables = db.list_tables().expect("list tables");
    assert_eq!(tables.len(), 2);
}

#[test]
fn test_catalog_stable_table_ids_across_restarts() {
    let (_dir, path) = temp_db_path();

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        let t1 = db.create_table("t1").expect("create t1");
        let t2 = db.create_table("t2").expect("create t2");
        assert_eq!(t1, TableId(1));
        assert_eq!(t2, TableId(2));

        db.drop_table("t1").expect("drop t1");
        db.sync().expect("sync");
    }

    // Reopen database and create t3
    {
        let mut db = TreeDb::open(&path, 10).expect("reopen database");
        let t3 = db.create_table("t3").expect("create t3");
        // IDs must be monotonic and NOT reused
        assert_eq!(t3, TableId(3));
        db.sync().expect("sync");
    }

    // Reopen again and verify table IDs remain stable
    let mut db = TreeDb::open(&path, 10).expect("reopen again");
    let t2_meta = db.get_table("t2").expect("get t2").expect("t2 exists");
    assert_eq!(t2_meta.table_id, TableId(2));

    let t3_meta = db.get_table("t3").expect("get t3").expect("t3 exists");
    assert_eq!(t3_meta.table_id, TableId(3));

    db.validate_catalog().expect("validate catalog");
}

// ============================================================================
// 3. Transaction Integration Tests
// ============================================================================

#[test]
fn test_catalog_transaction_create_commit_and_abort() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    // 1. Transaction Create -> Abort
    {
        let mut txn = db.begin().expect("begin txn");
        let tid = txn.create_table("aborted_table").expect("create table");
        assert_eq!(tid, TableId(1));
        assert!(txn.get_table("aborted_table").expect("get").is_some());
        txn.abort().expect("abort txn");
    }

    assert_eq!(
        db.get_table("aborted_table").expect("get after abort"),
        None
    );
    assert_eq!(db.list_tables().expect("list after abort").len(), 0);
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    // 2. Transaction Create -> Commit
    {
        let mut txn = db.begin().expect("begin txn");
        let tid = txn.create_table("committed_table").expect("create table");
        assert!(tid.0 >= 1);
        txn.commit().expect("commit txn");
    }

    assert!(db
        .get_table("committed_table")
        .expect("get committed")
        .is_some());
    db.validate_catalog().expect("validate catalog");
}

#[test]
fn test_catalog_transaction_drop_commit_and_abort() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("keep_me").expect("create keep_me");

    // 1. Drop -> Abort
    {
        let mut txn = db.begin().expect("begin txn");
        txn.drop_table("keep_me").expect("drop table in txn");
        assert_eq!(txn.get_table("keep_me").expect("get in txn"), None);
        txn.abort().expect("abort txn");
    }

    assert!(db.get_table("keep_me").expect("get after abort").is_some());
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    // 2. Drop -> Commit
    {
        let mut txn = db.begin().expect("begin txn");
        txn.drop_table("keep_me").expect("drop table in txn");
        txn.commit().expect("commit txn");
    }

    assert_eq!(db.get_table("keep_me").expect("get after commit"), None);
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

#[test]
fn test_catalog_transaction_drop_auto_rollback() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("persistent_table")
        .expect("create persistent");

    {
        let mut txn = db.begin().expect("begin txn");
        txn.create_table("temp_table").expect("create temp");
        txn.drop_table("persistent_table").expect("drop persistent");
        // Transaction dropped without commit -> auto-abort
    }

    assert!(db
        .get_table("persistent_table")
        .expect("get persistent")
        .is_some());
    assert_eq!(db.get_table("temp_table").expect("get temp"), None);
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

#[test]
fn test_catalog_transaction_multiple_operations_atomicity() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("t1").expect("create t1");
    db.create_table("t2").expect("create t2");

    // Multi-operation transaction abort
    {
        let mut txn = db.begin().expect("begin txn");
        txn.create_table("t3").expect("create t3");
        txn.drop_table("t1").expect("drop t1");
        txn.create_table("t4").expect("create t4");
        txn.drop_table("t2").expect("drop t2");
        txn.abort().expect("abort txn");
    }

    assert!(db.get_table("t1").expect("get t1").is_some());
    assert!(db.get_table("t2").expect("get t2").is_some());
    assert_eq!(db.get_table("t3").expect("get t3"), None);
    assert_eq!(db.get_table("t4").expect("get t4"), None);

    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

// ============================================================================
// 4. Crash Recovery Tests
// ============================================================================

#[test]
fn test_catalog_crash_recovery_uncommitted_create() {
    let (_dir, path) = temp_db_path();

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        db.create_table("persisted_table")
            .expect("create persisted");

        let mut txn = db.begin().expect("begin txn");
        txn.create_table("crashed_uncommitted_table")
            .expect("create table");
        // Simulate sudden crash: do not call commit, forget txn to bypass Drop
        std::mem::forget(txn);
    }

    // Crash recovery on reopen
    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    assert!(db
        .get_table("persisted_table")
        .expect("get persisted")
        .is_some());
    assert_eq!(
        db.get_table("crashed_uncommitted_table")
            .expect("get uncommitted"),
        None
    );
}

#[test]
fn test_catalog_crash_recovery_committed_create() {
    let (_dir, path) = temp_db_path();

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        let mut txn = db.begin().expect("begin txn");
        txn.create_table("committed_crash_table")
            .expect("create table");
        txn.commit().expect("commit");
    }

    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");

    let table = db
        .get_table("committed_crash_table")
        .expect("get table")
        .expect("table must exist");
    assert_eq!(table.name, "committed_crash_table");
}

#[test]
fn test_catalog_crash_recovery_uncommitted_drop() {
    let (_dir, path) = temp_db_path();

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        db.create_table("surviving_table")
            .expect("create surviving");

        let mut txn = db.begin().expect("begin txn");
        txn.drop_table("surviving_table")
            .expect("drop table in txn");
        // Simulate crash
        std::mem::forget(txn);
    }

    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");

    assert!(db
        .get_table("surviving_table")
        .expect("get surviving")
        .is_some());
}

#[test]
fn test_catalog_crash_recovery_committed_drop() {
    let (_dir, path) = temp_db_path();

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        db.create_table("dropped_table").expect("create table");

        let mut txn = db.begin().expect("begin txn");
        txn.drop_table("dropped_table").expect("drop table in txn");
        txn.commit().expect("commit drop");
    }

    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    assert_eq!(db.get_table("dropped_table").expect("get dropped"), None);
}

// ============================================================================
// 5. Checkpoint & WAL Truncation Tests
// ============================================================================

#[test]
fn test_catalog_checkpoint_and_wal_truncation() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("t1").expect("create t1");
    db.checkpoint_and_truncate_wal().expect("checkpoint 1");

    db.create_table("t2").expect("create t2");
    db.checkpoint_and_truncate_wal().expect("checkpoint 2");

    db.drop_table("t1").expect("drop t1");
    db.checkpoint_and_truncate_wal().expect("checkpoint 3");

    drop(db);

    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    assert_eq!(db.get_table("t1").expect("get t1"), None);
    assert!(db.get_table("t2").expect("get t2").is_some());
}

// ============================================================================
// 6. Freelist Integration & Recycling Tests
// ============================================================================

#[test]
fn test_catalog_freelist_page_recycling_on_drop() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let initial_free = db.buffer_pool().page_manager().header().free_page_count;

    let t1 = db.create_table("table_one").expect("create table_one");
    let meta1 = db.get_table_by_id(t1).unwrap().unwrap();

    // Dropping table_one must free its root_page_id and record_page_id
    db.drop_table("table_one").expect("drop table_one");

    let free_after_drop = db.buffer_pool().page_manager().header().free_page_count;
    assert_eq!(free_after_drop, initial_free + 2);

    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    // Creating a new table should reuse the freed pages from the freelist
    let t2 = db.create_table("table_two").expect("create table_two");
    let meta2 = db.get_table_by_id(t2).unwrap().unwrap();

    // One of the allocated pages must be reused from meta1
    assert!(
        meta2.root_page_id == meta1.record_page_id
            || meta2.root_page_id == meta1.root_page_id
            || meta2.record_page_id == meta1.record_page_id
            || meta2.record_page_id == meta1.root_page_id
    );

    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

// ============================================================================
// 7. Multi-Page Catalog Expansion Tests
// ============================================================================

#[test]
fn test_catalog_multi_page_chain_expansion() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 50).expect("create database");

    // Create 60 tables to exceed single-page catalog payload capacity (~4048 bytes)
    let num_tables = 60;
    for i in 0..num_tables {
        let name = format!("table_num_{:03}", i);
        db.create_table(&name)
            .unwrap_or_else(|e| panic!("create {name} failed: {e:?}"));
    }

    db.validate_catalog().expect("validate catalog");

    let tables = db.list_tables().expect("list all tables");
    assert_eq!(tables.len(), num_tables);

    for i in 0..num_tables {
        let name = format!("table_num_{:03}", i);
        let meta = db.get_table(&name).unwrap().expect("table must exist");
        assert_eq!(meta.name, name);
    }

    // Drop every other table
    for i in (0..num_tables).step_by(2) {
        let name = format!("table_num_{:03}", i);
        db.drop_table(&name)
            .unwrap_or_else(|e| panic!("drop {name} failed: {e:?}"));
    }

    db.validate_catalog().expect("validate catalog after drops");
    let remaining = db.list_tables().expect("list remaining");
    assert_eq!(remaining.len(), num_tables / 2);

    drop(db);

    // Reopen and verify multi-page chain persistence
    let mut db = TreeDb::open(&path, 50).expect("reopen database");
    db.validate_catalog().expect("validate catalog on reopen");
    let remaining_after = db.list_tables().expect("list remaining after reopen");
    assert_eq!(remaining_after.len(), num_tables / 2);
}

// ============================================================================
// 8. Corruption Detection Matrix Tests
// ============================================================================

#[test]
fn test_catalog_corruption_detection_matrix() {
    let (_dir, path) = temp_db_path();

    // 1. Invalid Magic
    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        db.create_table("valid_table").expect("create table");
        db.sync().expect("sync");

        // Corrupt magic on Page 1
        let mut pm = PageManager::open(&path).expect("open pm");
        let mut p1 = pm.read_page(DEFAULT_METADATA_PAGE_ID).expect("read p1");
        p1.payload_mut()[0..4].copy_from_slice(b"BADM");
        pm.write_page(&p1).expect("write corrupted p1");
    }

    {
        let mut db = TreeDb::open(&path, 10).expect("open");
        let err = db.validate_catalog().unwrap_err();
        assert!(matches!(err, Error::CatalogCorruption(_)));
    }

    // 2. Unsupported Version
    {
        let mut pm = PageManager::open(&path).expect("open pm");
        let mut p1 = pm.read_page(DEFAULT_METADATA_PAGE_ID).expect("read p1");
        p1.payload_mut()[0..4].copy_from_slice(CATALOG_MAGIC);
        p1.payload_mut()[4..8].copy_from_slice(&99u32.to_le_bytes()); // invalid version
        pm.write_page(&p1).expect("write corrupted version");
    }

    {
        let mut db = TreeDb::open(&path, 10).expect("open");
        let err = db.validate_catalog().unwrap_err();
        assert!(matches!(err, Error::CatalogVersionUnsupported { .. }));
    }

    // 3. Out-of-bounds Page Pointer
    {
        let mut pm = PageManager::open(&path).expect("open pm");
        let mut p1 = pm.read_page(DEFAULT_METADATA_PAGE_ID).expect("read p1");
        p1.payload_mut()[4..8].copy_from_slice(&CURRENT_CATALOG_VERSION.to_le_bytes());
        // Set root_page_id of first entry (offset 32 + 8 = 40) to out of bounds 9999
        let root_off = CATALOG_HEADER_SIZE + 8;
        p1.payload_mut()[root_off..root_off + 4].copy_from_slice(&9999u32.to_le_bytes());
        pm.write_page(&p1).expect("write corrupted root pointer");
    }

    {
        let mut db = TreeDb::open(&path, 10).expect("open");
        let err = db.validate_catalog().unwrap_err();
        assert!(matches!(err, Error::CatalogCorruption(_)));
    }
}

// ============================================================================
// 9. Randomized Catalog Oracle Comparison Test
// ============================================================================

#[test]
fn test_catalog_randomized_oracle_comparison() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 50).expect("create database");

    let mut oracle: BTreeMap<String, TableMetadata> = BTreeMap::new();
    let mut lcg: u64 = 0xCA7A_1060_DEAD_BEEF;
    let mut next_rand = || {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
        lcg
    };

    let op_count = 100;

    for step in 0..op_count {
        let op_type = next_rand() % 4;

        match op_type {
            // Op 0: Create table
            0 => {
                let name = format!("rnd_tbl_{}", next_rand() % 25);
                let should_succeed = !oracle.contains_key(&name);

                let is_txn = (next_rand() % 2) == 1;
                if is_txn {
                    let mut txn = db.begin().expect("begin txn");
                    let res = txn.create_table(&name);
                    if should_succeed {
                        let tid = res.expect("create table in txn should succeed");
                        let meta = txn.get_table(&name).unwrap().unwrap();
                        assert_eq!(meta.table_id, tid);

                        let commit = (next_rand() % 10) < 7;
                        if commit {
                            txn.commit().expect("commit");
                            oracle.insert(name, meta);
                        } else {
                            txn.abort().expect("abort");
                        }
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::TableAlreadyExists(_)));
                        txn.abort().expect("abort");
                    }
                } else {
                    let res = db.create_table(&name);
                    if should_succeed {
                        let tid = res.expect("create table should succeed");
                        let meta = db.get_table(&name).unwrap().unwrap();
                        assert_eq!(meta.table_id, tid);
                        oracle.insert(name, meta);
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::TableAlreadyExists(_)));
                    }
                }
            }

            // Op 1: Drop table
            1 => {
                let name = format!("rnd_tbl_{}", next_rand() % 25);
                let should_succeed = oracle.contains_key(&name);

                let is_txn = (next_rand() % 2) == 1;
                if is_txn {
                    let mut txn = db.begin().expect("begin txn");
                    let res = txn.drop_table(&name);
                    if should_succeed {
                        res.expect("drop table in txn should succeed");
                        let commit = (next_rand() % 10) < 7;
                        if commit {
                            txn.commit().expect("commit");
                            oracle.remove(&name);
                        } else {
                            txn.abort().expect("abort");
                        }
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::TableNotFound(_)));
                        txn.abort().expect("abort");
                    }
                } else {
                    let res = db.drop_table(&name);
                    if should_succeed {
                        res.expect("drop table should succeed");
                        oracle.remove(&name);
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::TableNotFound(_)));
                    }
                }
            }

            // Op 2: Lookup table
            2 => {
                let name = format!("rnd_tbl_{}", next_rand() % 25);
                let expected = oracle.get(&name);
                let actual = db.get_table(&name).expect("get table");
                assert_eq!(actual.as_ref(), expected);
            }

            // Op 3: List tables and validate
            _ => {
                let list = db.list_tables().expect("list tables");
                assert_eq!(list.len(), oracle.len());
                for meta in &list {
                    assert_eq!(oracle.get(&meta.name), Some(meta));
                }
                db.validate_catalog().expect("validate catalog");
            }
        }

        // Periodic checkpoint and truncation
        if step % 25 == 0 {
            db.checkpoint_and_truncate_wal().expect("checkpoint");
        }
    }

    // Final verification against oracle
    let final_list = db.list_tables().expect("final list");
    assert_eq!(final_list.len(), oracle.len());
    for meta in &final_list {
        assert_eq!(oracle.get(&meta.name), Some(meta));
    }

    db.validate_catalog().expect("final validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("final validate freelist");
}
