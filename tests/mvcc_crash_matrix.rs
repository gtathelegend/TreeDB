//! Phase 69: Deterministic MVCC Crash-Injection Failure Matrix.
//!
//! Validates transactional durability, version-chain recovery, index consistency,
//! and overflow chain preservation across the 8 deterministic failure boundaries:
//! 1. Before first persistent mutation.
//! 2. After WAL record is appended but before it is durable.
//! 3. After data-page change.
//! 4. After version-chain metadata changes.
//! 5. After index maintenance.
//! 6. After commit record becomes durable.
//! 7. During recovery (using `RecoveryFault`).
//! 8. After recovery but before a subsequent checkpoint.

use tempfile::tempdir;
use treedb::engine::isolation::{
    CommitSeq, IsolationContext, IsolationLevel, Snapshot, SnapshotId,
};
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::storage::page_manager::PageManager;
use treedb::wal::log_manager::LogManager;
use treedb::wal::lsn::TxnId;
use treedb::wal::recovery::{RecoveryEngine, RecoveryFault};
use treedb::TreeDb;

fn make_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("sku", DataType::String, false),
            Column::new("details", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn create_matrix_db(name: &str) -> (TreeDb, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir, db_path)
}

// -----------------------------------------------------------------------------
// Boundary 1: Failure before first persistent mutation
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_1_before_first_mutation() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b1");
    db.create_table("items", make_test_schema()).unwrap();

    let seq_before = db.buffer_pool().durable_max_commit_seq();

    // Start transaction, do nothing, abort / terminate
    let t = db.begin().unwrap();
    drop(t); // Terminate without mutations
    drop(db);

    // Reopen and verify
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(reopened.buffer_pool().durable_max_commit_seq(), seq_before);
    let mut tbl = reopened.open_table("items").unwrap();
    let rows = tbl.scan(None, None).unwrap();
    assert!(rows.is_empty(), "No rows should exist");
}

// -----------------------------------------------------------------------------
// Boundary 2: Failure after WAL record appended but before flush/durable
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_2_unflushed_wal_tail() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b2");
    db.create_table("items", make_test_schema()).unwrap();

    // Commit baseline row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("SKU-1".into()),
            Value::Bytes(vec![1, 2, 3]),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    // Begin uncommitted insert
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("SKU-2".into()),
            Value::Bytes(vec![4, 5, 6]),
        ]))
        .unwrap();
    }
    // Simulate abrupt crash by dropping without commit or flush
    std::mem::forget(t1);
    drop(db);

    // Reopen and run ARIES recovery
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    let mut tbl = reopened.open_table("items").unwrap();
    assert!(
        tbl.get(&Value::Int64(1)).unwrap().is_some(),
        "Committed row 1 must survive"
    );
    assert!(
        tbl.get(&Value::Int64(2)).unwrap().is_none(),
        "Unflushed uncommitted row 2 must not be visible"
    );
}

// -----------------------------------------------------------------------------
// Boundary 3: Failure after data-page mutation before commit record
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_3_data_page_modified_uncommitted() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b3");
    db.create_table("items", make_test_schema()).unwrap();

    // Insert initial row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("SKU-10-OLD".into()),
            Value::Bytes(vec![10]),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    // Update row in active transaction, but do not commit
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("items").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(10),
            Value::String("SKU-10-NEW".into()),
            Value::Bytes(vec![99]),
        ]))
        .unwrap();
    }
    // Simulate crash before commit
    std::mem::forget(t1);
    drop(db);

    // Reopen and verify row 10 retains its committed state
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    let mut tbl = reopened.open_table("items").unwrap();
    let row = tbl.get(&Value::Int64(10)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String("SKU-10-OLD".into()));
    assert_eq!(row.values[2], Value::Bytes(vec![10]));
}

// -----------------------------------------------------------------------------
// Boundary 4: Failure after version-chain metadata update before commit
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_4_version_chain_metadata_abort_recovery() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b4");
    db.create_table("items", make_test_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(20),
            Value::String("ORIGINAL".into()),
            Value::Bytes(vec![1, 1, 1]),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    // Uncommitted delete marks xmax on old version
    let mut t_del = db.begin().unwrap();
    {
        let mut tbl = t_del.open_table("items").unwrap();
        let deleted = tbl.delete(&Value::Int64(20)).unwrap();
        assert!(deleted.is_some());
    }
    // Crash before commit
    std::mem::forget(t_del);
    drop(db);

    // Reopen: deleted row must remain visible because deletion never committed
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    let mut tbl = reopened.open_table("items").unwrap();
    let row = tbl.get(&Value::Int64(20)).unwrap();
    assert!(
        row.is_some(),
        "Uncommitted delete must be undone by recovery; row must remain visible"
    );
    assert_eq!(row.unwrap().values[1], Value::String("ORIGINAL".into()));
}

// -----------------------------------------------------------------------------
// Boundary 5: Failure after index maintenance before commit
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_5_index_maintenance_uncommitted() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b5");
    db.create_table("items", make_test_schema()).unwrap();
    db.execute_statement("CREATE INDEX idx_items_sku ON items (sku);")
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(30),
            Value::String("SKU-30-A".into()),
            Value::Bytes(vec![1]),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    // Update changing indexed value
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("items").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(30),
            Value::String("SKU-30-B".into()),
            Value::Bytes(vec![2]),
        ]))
        .unwrap();
    }
    // Crash before commit
    std::mem::forget(t1);
    drop(db);

    // Reopen: index lookup on old SKU must succeed; index lookup on new SKU must return empty
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    let mut tbl = reopened.open_table("items").unwrap();
    let old_sku_res = tbl
        .index_lookup("idx_items_sku", &Value::String("SKU-30-A".into()))
        .unwrap();
    assert_eq!(old_sku_res.len(), 1, "Old SKU must remain indexed");
    assert_eq!(old_sku_res[0].values[0], Value::Int64(30));

    let new_sku_res = tbl
        .index_lookup("idx_items_sku", &Value::String("SKU-30-B".into()))
        .unwrap();
    assert_eq!(
        new_sku_res.len(),
        0,
        "Uncommitted new SKU must not appear in index"
    );
}

// -----------------------------------------------------------------------------
// Boundary 6: Failure after commit record becomes durable
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_6_commit_record_durable_survives() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b6");
    db.create_table("items", make_test_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("items").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(40),
            Value::String("COMMITTED-40".into()),
            Value::Bytes(vec![40]),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();
    let seq_at_commit = db.buffer_pool().durable_max_commit_seq();

    // Drop immediately simulating sudden crash after commit
    drop(db);

    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(
        reopened.buffer_pool().durable_max_commit_seq(),
        seq_at_commit
    );
    let mut tbl = reopened.open_table("items").unwrap();
    let row = tbl.get(&Value::Int64(40)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String("COMMITTED-40".into()));
}

// -----------------------------------------------------------------------------
// Boundary 7: Failure during recovery (simulated via RecoveryFault)
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_7_failure_during_recovery_restart() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("crash_b7.treedb");
    let wal_path = std::path::PathBuf::from(format!("{}.wal", db_path.display()));

    // Phase 1: create db, write mutations, leave uncommitted txn
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let mut t1 = db.begin().unwrap();
        t1.put(100, b"val_100").unwrap();
        t1.commit().unwrap();

        let mut t2 = db.begin().unwrap();
        t2.put(200, b"uncommitted_val").unwrap();
        std::mem::forget(t2);
    }

    // Phase 2: run recovery with RecoveryFault::BeforeUndo -> must return error deterministically
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut wal = LogManager::open(&wal_path).unwrap();
        let res =
            RecoveryEngine::recover_with_fault(&mut pm, &mut wal, Some(RecoveryFault::BeforeUndo));
        assert!(res.is_err(), "Recovery fault must trigger expected error");
    }

    // Phase 3: run standard recovery on the interrupted state -> must cleanly complete
    {
        let mut reopened = TreeDb::open(&db_path, 64).unwrap();
        let val100 = reopened.get(100).unwrap();
        assert_eq!(val100, Some(b"val_100".to_vec()));
        let val200 = reopened.get(200).unwrap();
        assert_eq!(
            val200, None,
            "Uncommitted key 200 must be absent after clean recovery"
        );
    }
}

// -----------------------------------------------------------------------------
// Boundary 8: Failure after recovery but before subsequent checkpoint
// -----------------------------------------------------------------------------
#[test]
fn test_crash_boundary_8_post_recovery_pre_checkpoint() {
    let (mut db, _dir, db_path) = create_matrix_db("crash_b8");

    // Commit key 1
    db.put(1, b"stable_1").unwrap();

    // Reopen (triggers recovery)
    drop(db);
    let mut reopened1 = TreeDb::open(&db_path, 64).unwrap();
    // Do a new mutation without checkpoint
    reopened1.put(2, b"post_recovery_2").unwrap();

    // Abrupt termination again before checkpoint
    drop(reopened1);

    // Reopen second time
    let mut reopened2 = TreeDb::open(&db_path, 64).unwrap();
    assert_eq!(reopened2.get(1).unwrap(), Some(b"stable_1".to_vec()));
    assert_eq!(reopened2.get(2).unwrap(), Some(b"post_recovery_2".to_vec()));
}

// -----------------------------------------------------------------------------
// Large Multi-Page Overflow Crash & Recovery Matrix
// -----------------------------------------------------------------------------
#[test]
fn test_overflow_multi_page_chain_crash_recovery_matrix() {
    let (mut db, _dir, db_path) = create_matrix_db("overflow_crash_matrix");
    db.create_table("large_docs", make_test_schema()).unwrap();

    let large_payload_1 = vec![0xAAu8; 12000]; // Multi-page overflow
    let large_payload_2 = vec![0xBBu8; 15000]; // Larger multi-page overflow

    // V1 committed
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("large_docs").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("DOC-1".into()),
            Value::Bytes(large_payload_1.clone()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();
    let snap_v1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // V2 update committed
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("large_docs").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("DOC-1".into()),
            Value::Bytes(large_payload_2.clone()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();
    let snap_v2 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Uncommitted update with V3 -> crashed
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("large_docs").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("DOC-1".into()),
            Value::Bytes(vec![0xCCu8; 20000]),
        ]))
        .unwrap();
    }
    std::mem::forget(t3);
    drop(db);

    // Reopen and verify historical overflow version preservation
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    {
        let mut tbl = reopened.open_table("large_docs").unwrap();

        // Historical snapshot 1 sees complete V1 payload
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(501),
            Snapshot::new(SnapshotId(1), snap_v1),
        )));
        let row1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row1.values[2], Value::Bytes(large_payload_1));

        // Snapshot 2 sees complete V2 payload
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(502),
            Snapshot::new(SnapshotId(2), snap_v2),
        )));
        let row2 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row2.values[2], Value::Bytes(large_payload_2));

        // Current snapshot also sees V2 (since V3 crashed uncommitted)
        tbl.set_isolation_context(None);
        let row_curr = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row_curr.values[2], Value::Bytes(vec![0xBBu8; 15000]));
    }
}
