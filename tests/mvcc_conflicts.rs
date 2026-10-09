//! Phase 71: Transaction Conflict Detection & Commit-Time Validation.
//!
//! Formal verification of TreeDB's MVCC conflict-detection contract and commit-validation layer:
//! - Section A: Write-write conflicts
//! - Section B: Stale deletes
//! - Section C: Multiple operations within one transaction
//! - Section D: Primary-key operations
//! - Section E: Unique secondary indexes
//! - Section F: Transaction state transitions and write-set validation
//! - Section G: Crash recovery and retry behavior

use tempfile::tempdir;
use treedb::engine::isolation::{
    CommitSeq, Snapshot, SnapshotId, TransactionConflict, WriteOpKind,
};
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::{TreeDb, TxnState};

fn make_accounts_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("account_num", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
        ],
        0,
    )
    .unwrap()
}

fn create_test_db(name: &str) -> (TreeDb, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir, db_path)
}

// =============================================================================
// Section A: Write-Write Conflicts
// =============================================================================

#[test]
fn test_write_write_conflict_raw_kv() {
    let (mut db, _dir, _path) = create_test_db("ww_raw_kv");

    // Seed key 10 with v1
    let mut t0 = db.begin().unwrap();
    t0.put(10, b"v1").unwrap();
    t0.commit().unwrap();

    let seq_at_v1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Transaction 1 updates key 10 to v2 and commits
    let mut t1 = db.begin().unwrap();
    t1.put(10, b"v2").unwrap();
    t1.commit().unwrap();

    // Transaction 2 started with snapshot at v1
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(99), seq_at_v1));

    // Transaction 2 observes v1 under its snapshot
    assert_eq!(t2.get(10).unwrap(), Some(b"v1".to_vec()));

    // Transaction 2 attempts to update key 10 -> detected as WriteWriteConflict
    let err = t2.put(10, b"v3_conflict").unwrap_err();
    assert!(err.is_conflict());
    match err {
        Error::TransactionConflict(TransactionConflict::WriteWriteConflict {
            table, key, ..
        }) => {
            assert_eq!(table, "__kv__");
            assert_eq!(key, "10");
        }
        other => panic!("expected WriteWriteConflict, got: {other:?}"),
    }

    // Transaction 2 cannot commit after conflict
    let commit_err = t2.commit().unwrap_err();
    assert!(commit_err.is_conflict());

    // Final committed row remains v2
    let mut verify = db.begin().unwrap();
    assert_eq!(verify.get(10).unwrap(), Some(b"v2".to_vec()));
    verify.commit().unwrap();
}

#[test]
fn test_write_write_conflict_relational_table() {
    let (mut db, _dir, _path) = create_test_db("ww_relational");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Seed account 1
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(100),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    let seq_seed = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Intervening transaction commits balance update to 200
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(200),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Transaction 2 with snapshot prior to t1 attempts update
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(88), seq_seed));

    // t2 observes balance = 100 under snapshot
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[2], Value::Int64(100));

        // Attempt stale update based on balance = 100
        let err = tbl
            .update(&Row::new(vec![
                Value::Int64(1),
                Value::String("ACC-001".into()),
                Value::Int64(150),
            ]))
            .unwrap_err();

        assert!(err.is_conflict());
        match err {
            Error::TransactionConflict(TransactionConflict::WriteWriteConflict {
                table,
                key,
                ..
            }) => {
                assert_eq!(table, "accounts");
                assert_eq!(key, "1");
            }
            other => panic!("expected WriteWriteConflict, got: {other:?}"),
        }
    }

    // Commit fails
    let commit_err = t2.commit().unwrap_err();
    assert!(commit_err.is_conflict());

    // Database state remains 200
    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[2], Value::Int64(200));
    }
    check.commit().unwrap();
}

// =============================================================================
// Section B: Stale Deletes
// =============================================================================

#[test]
fn test_stale_delete_rejected_after_concurrent_update() {
    let (mut db, _dir, _path) = create_test_db("stale_delete_update");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Seed account 1
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(500),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    let seq_seed = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Transaction 1 updates balance to 600
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(600),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Transaction 2 tries to delete row 1 based on older snapshot
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(77), seq_seed));

    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let err = tbl.delete(&Value::Int64(1)).unwrap_err();
        assert!(err.is_conflict());
        match err {
            Error::TransactionConflict(TransactionConflict::WriteWriteConflict {
                table,
                key,
                ..
            }) => {
                assert_eq!(table, "accounts");
                assert_eq!(key, "1");
            }
            other => panic!("expected WriteWriteConflict for stale delete, got: {other:?}"),
        }
    }

    let _ = t2.abort();

    // Row 1 is still present with balance 600
    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[2], Value::Int64(600));
    }
    check.commit().unwrap();
}

#[test]
fn test_stale_delete_rejected_after_concurrent_delete() {
    let (mut db, _dir, _path) = create_test_db("stale_delete_delete");

    let mut t0 = db.begin().unwrap();
    t0.put(42, b"val42").unwrap();
    t0.commit().unwrap();

    let seq_seed = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Intervening delete commits
    let mut t1 = db.begin().unwrap();
    assert_eq!(t1.delete(42).unwrap(), Some(b"val42".to_vec()));
    t1.commit().unwrap();

    // Older transaction attempts delete
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(66), seq_seed));

    let err = t2.delete(42).unwrap_err();
    assert!(err.is_conflict());
    match err {
        Error::TransactionConflict(TransactionConflict::StaleVersionWrite {
            table, key, ..
        }) => {
            assert_eq!(table, "__kv__");
            assert_eq!(key, "42");
        }
        other => panic!("expected StaleVersionWrite, got: {other:?}"),
    }

    let _ = t2.abort();
}

// =============================================================================
// Section C: Multiple Operations Within One Transaction
// =============================================================================

#[test]
fn test_multiple_operations_within_single_transaction() {
    let (mut db, _dir, _path) = create_test_db("intra_txn_ops");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc_num", "account_num", true)
        .unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();

        // 1. Insert row 1
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(100),
        ]))
        .unwrap();

        // 2. Update row 1
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(200),
        ]))
        .unwrap();

        // 3. Update row 1 again
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(300),
        ]))
        .unwrap();

        // Immediate read sees final update
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[2], Value::Int64(300));
    }

    // Verify write set tracks intents cleanly
    assert_eq!(txn.write_set().intents().len(), 1);
    let intent = txn.write_set().intents().values().next().unwrap();
    assert_eq!(intent.table, "accounts");
    assert_eq!(intent.key_repr, "1");
    assert_eq!(intent.op_kind, WriteOpKind::Insert); // Originally inserted in this txn!

    // Commit succeeds
    txn.commit().unwrap();

    // Verify committed row and secondary index
    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[2], Value::Int64(300));

        let idx_rows = tbl
            .index_lookup("idx_acc_num", &Value::String("ACC-001".into()))
            .unwrap();
        assert_eq!(idx_rows.len(), 1);
        assert_eq!(idx_rows[0].values[2], Value::Int64(300));
    }
    check.commit().unwrap();
}

#[test]
fn test_insert_then_delete_intra_transaction() {
    let (mut db, _dir, _path) = create_test_db("insert_delete_intra");

    let mut txn = db.begin().unwrap();
    txn.put(100, b"temp").unwrap();
    assert_eq!(txn.get(100).unwrap(), Some(b"temp".to_vec()));

    let del = txn.delete(100).unwrap();
    assert_eq!(del, Some(b"temp".to_vec()));
    assert_eq!(txn.get(100).unwrap(), None);

    // Commit transaction with net no-op
    txn.commit().unwrap();

    let mut check = db.begin().unwrap();
    assert_eq!(check.get(100).unwrap(), None);
    check.commit().unwrap();
}

#[test]
fn test_update_then_delete_intra_transaction() {
    let (mut db, _dir, _path) = create_test_db("update_delete_intra");

    let mut t0 = db.begin().unwrap();
    t0.put(200, b"orig").unwrap();
    t0.commit().unwrap();

    let mut txn = db.begin().unwrap();
    txn.put(200, b"updated").unwrap();
    assert_eq!(txn.get(200).unwrap(), Some(b"updated".to_vec()));

    txn.delete(200).unwrap();
    assert_eq!(txn.get(200).unwrap(), None);

    txn.commit().unwrap();

    let mut check = db.begin().unwrap();
    assert_eq!(check.get(200).unwrap(), None);
    check.commit().unwrap();
}

// =============================================================================
// Section D: Primary-Key Operations
// =============================================================================

#[test]
fn test_primary_key_duplicate_insert_and_key_reuse() {
    let (mut db, _dir, _path) = create_test_db("pk_reuse");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Insert account 1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-001".into()),
            Value::Int64(100),
        ]))
        .unwrap();

        // Duplicate insert in same txn fails
        let err = tbl
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("ACC-002".into()),
                Value::Int64(200),
            ]))
            .unwrap_err();
        assert!(matches!(err, Error::DuplicatePrimaryKey(_)));
    }
    t1.commit().unwrap();

    // Delete account 1
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let deleted = tbl.delete(&Value::Int64(1)).unwrap();
        assert!(deleted.is_some());
    }
    t2.commit().unwrap();

    // Key reuse: Insert account 1 again with new details
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-REUSED".into()),
            Value::Int64(999),
        ]))
        .unwrap();
    }
    t3.commit().unwrap();

    // Verify key reuse succeeded
    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[1], Value::String("ACC-REUSED".into()));
        assert_eq!(r.values[2], Value::Int64(999));
    }
    check.commit().unwrap();
}

#[test]
fn test_primary_key_aborted_key_reuse() {
    let (mut db, _dir, _path) = create_test_db("pk_abort_reuse");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Transaction inserts account 10 and aborts
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("ACC-010".into()),
            Value::Int64(100),
        ]))
        .unwrap();
    }
    t1.abort().unwrap();

    // Subsequent transaction can cleanly insert account 10
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("ACC-010-NEW".into()),
            Value::Int64(200),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(10)).unwrap().unwrap();
        assert_eq!(r.values[1], Value::String("ACC-010-NEW".into()));
    }
    check.commit().unwrap();
}

#[test]
fn test_concurrent_logical_key_change_detected() {
    let (mut db, _dir, _path) = create_test_db("concurrent_pk");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let snap_empty = CommitSeq::ZERO;

    // Intervening transaction inserts account 5
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(5),
            Value::String("ACC-005".into()),
            Value::Int64(50),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Older transaction with snapshot before t1 attempts to insert account 5
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(55), snap_empty));

    {
        let mut tbl = t2.open_table("accounts").unwrap();
        // Under its snapshot, key 5 is not visible. But physical insert detects intervening committed PK!
        let err = tbl
            .insert(&Row::new(vec![
                Value::Int64(5),
                Value::String("ACC-005-CONFLICT".into()),
                Value::Int64(150),
            ]))
            .unwrap_err();

        assert!(err.is_conflict());
        match err {
            Error::TransactionConflict(TransactionConflict::ConcurrentLogicalKeyChange {
                table,
                key,
                ..
            }) => {
                assert_eq!(table, "accounts");
                assert_eq!(key, "5");
            }
            other => panic!("expected ConcurrentLogicalKeyChange, got: {other:?}"),
        }
    }
    let _ = t2.abort();
}

// =============================================================================
// Section E: Unique Secondary Indexes
// =============================================================================

#[test]
fn test_unique_secondary_index_conflict_intervening_insert() {
    let (mut db, _dir, _path) = create_test_db("unique_idx_conflict");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc_num", "account_num", true)
        .unwrap();

    let snap_empty = CommitSeq::ZERO;

    // Transaction 1 inserts account with ACC-UNIQUE
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-UNIQUE".into()),
            Value::Int64(100),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Transaction 2 (older snapshot) attempts to insert different PK with same ACC-UNIQUE
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(44), snap_empty));

    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let err = tbl
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("ACC-UNIQUE".into()),
                Value::Int64(200),
            ]))
            .unwrap_err();

        assert!(err.is_conflict());
        match err {
            Error::TransactionConflict(TransactionConflict::UniqueIndexConflict {
                index_name,
                key,
            }) => {
                assert_eq!(index_name, "idx_acc_num");
                assert!(key.contains("ACC-UNIQUE"));
            }
            other => panic!("expected UniqueIndexConflict, got: {other:?}"),
        }
    }
    let _ = t2.abort();
}

#[test]
fn test_unique_secondary_index_update_conflict() {
    let (mut db, _dir, _path) = create_test_db("unique_idx_update");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc_num", "account_num", true)
        .unwrap();

    // Seed rows 1 and 2
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-1".into()),
            Value::Int64(10),
        ]))
        .unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("ACC-2".into()),
            Value::Int64(20),
        ]))
        .unwrap();
    }
    t0.commit().unwrap();

    let snap_seed = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Transaction 1 updates row 2 to ACC-NEW
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(2),
            Value::String("ACC-NEW".into()),
            Value::Int64(25),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Transaction 2 (older snapshot) attempts to update row 1 to ACC-NEW
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(33), snap_seed));

    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let err = tbl
            .update(&Row::new(vec![
                Value::Int64(1),
                Value::String("ACC-NEW".into()),
                Value::Int64(15),
            ]))
            .unwrap_err();

        assert!(err.is_conflict());
        match err {
            Error::TransactionConflict(TransactionConflict::UniqueIndexConflict {
                index_name,
                key,
            }) => {
                assert_eq!(index_name, "idx_acc_num");
                assert!(key.contains("ACC-NEW"));
            }
            other => panic!("expected UniqueIndexConflict, got: {other:?}"),
        }
    }
    let _ = t2.abort();
}

#[test]
fn test_unique_secondary_index_reinsertion_after_delete_and_vacuum() {
    let (mut db, _dir, _path) = create_test_db("unique_idx_vac");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc_num", "account_num", true)
        .unwrap();

    // Insert row 1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-KEY".into()),
            Value::Int64(100),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Delete row 1
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    t2.commit().unwrap();

    // Vacuum table
    let mut tv = db.begin().unwrap();
    let vac_res = tv
        .vacuum_table("accounts", &treedb::engine::vacuum::VacuumConfig::default())
        .unwrap();
    assert!(vac_res.versions_reclaimed >= 1);
    tv.commit().unwrap();

    // Insert row 2 with same ACC-KEY
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("ACC-KEY".into()),
            Value::Int64(200),
        ]))
        .unwrap();
    }
    t3.commit().unwrap();

    // Verify row 2 is indexed and readable
    let mut check = db.begin().unwrap();
    {
        let mut tbl = check.open_table("accounts").unwrap();
        let rows = tbl
            .index_lookup("idx_acc_num", &Value::String("ACC-KEY".into()))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Int64(2));
    }
    check.commit().unwrap();
}

// =============================================================================
// Section F: Transaction State Transitions & Write-Set Validation
// =============================================================================

#[test]
fn test_transaction_lifecycle_transitions() {
    let (mut db, _dir, _path) = create_test_db("lifecycle");

    // 1. Commit twice
    let mut t1 = db.begin().unwrap();
    t1.put(1, b"v").unwrap();
    t1.commit_in_place().unwrap();
    assert_eq!(t1.state(), TxnState::Committed);

    let err_double_commit = t1.commit_in_place().unwrap_err();
    assert!(err_double_commit.is_conflict());

    let err_abort_after_commit = t1.abort_in_place().unwrap_err();
    assert!(err_abort_after_commit.is_conflict());
    drop(t1);

    // 2. Commit after abort
    let mut t2 = db.begin().unwrap();
    t2.put(2, b"v").unwrap();
    t2.abort_in_place().unwrap();
    assert_eq!(t2.state(), TxnState::Aborted);

    let err_commit_after_abort = t2.commit_in_place().unwrap_err();
    assert!(err_commit_after_abort.is_conflict());

    let err_abort_after_abort = t2.abort_in_place().unwrap_err();
    assert!(err_abort_after_abort.is_conflict());
}

#[test]
fn test_validate_commit_after_earlier_operation_fails() {
    let (mut db, _dir, _path) = create_test_db("validate_fail");

    let mut t0 = db.begin().unwrap();
    t0.put(10, b"v1").unwrap();
    t0.commit().unwrap();

    let seq_v1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    let mut t1 = db.begin().unwrap();
    t1.put(10, b"v2").unwrap();
    t1.commit().unwrap();

    // Transaction 2 tries stale update on key 10
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(Snapshot::new(SnapshotId(22), seq_v1));

    let op_err = t2.put(10, b"v3").unwrap_err();
    assert!(op_err.is_conflict());

    // Explicit validate_commit detects earlier operation failure
    let val_err = t2.validate_commit().unwrap_err();
    assert!(val_err.is_conflict());
    match val_err {
        Error::TransactionConflict(TransactionConflict::WriteSetValidationFailure {
            txn_id,
            reason,
        }) => {
            assert_eq!(txn_id, t2.id());
            assert!(reason.contains("conflict"));
        }
        other => panic!("expected WriteSetValidationFailure, got: {other:?}"),
    }

    // Calling commit automatically rolls back and marks aborted
    let commit_err = t2.commit().unwrap_err();
    assert!(commit_err.is_conflict());

    // Database still holds v2
    let mut check = db.begin().unwrap();
    assert_eq!(check.get(10).unwrap(), Some(b"v2".to_vec()));
    check.commit().unwrap();
}

// =============================================================================
// Section G: Recovery & Retry Behavior
// =============================================================================

#[test]
fn test_crash_recovery_uncommitted_modifications_and_retry() {
    let (mut db, _dir, db_path) = create_test_db("recovery_retry");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // 1. Uncommitted transaction
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-UNCOMMITTED".into()),
            Value::Int64(999),
        ]))
        .unwrap();
    }
    // Drop transaction without commit (simulating crash before commit)
    drop(t1);
    drop(db);

    // 2. Reopen database and verify recovery
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    {
        let mut check = reopened.begin().unwrap();
        let mut tbl = check.open_table("accounts").unwrap();
        assert_eq!(tbl.get(&Value::Int64(1)).unwrap(), None);
        check.commit().unwrap();
    }

    // 3. Subsequent insert (retry) succeeds cleanly
    let mut t2 = reopened.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-COMMITTED".into()),
            Value::Int64(100),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    // Verify row is durable and index is clean
    let mut final_check = reopened.begin().unwrap();
    {
        let mut tbl = final_check.open_table("accounts").unwrap();
        let r = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r.values[1], Value::String("ACC-COMMITTED".into()));
    }
    final_check.commit().unwrap();
}
