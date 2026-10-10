//! Phase 72: Transaction Atomicity Audit, WAL Failure Injection & Recovery Invariants.
//!
//! Validates:
//! 1. Complete commit protocol ordering and durable commit point invariants.
//! 2. Deterministic failure injection across all storage mutation and recovery boundaries.
//! 3. Transaction atomicity: all-or-nothing visibility for committed, aborted, and interrupted transactions.
//! 4. Recovery idempotency: repeated database reopening produces identical durable state.
//! 5. Structural consistency across Primary B+ Tree, Secondary Indexes, Record Store, Overflow Chains, and Freelist.
//! 6. MVCC historical version visibility and snapshot isolation under recovery.
//! 7. Snapshot-safe vacuum reclamation and crash interruption.
//! 8. Independent differential reference model with deterministic seed generation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::engine::vacuum::{VacuumConfig, VacuumEngine};
use treedb::{Error, StorageFailurePoint, TreeDb};

/// Failure classification distinguishing error kinds under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClassification {
    ExpectedConflict,
    OrdinaryExecutionError,
    ExplicitAbort,
    InjectedSimulatedCrash,
    RecoveryError,
    ConsistencyValidationFailure,
}

fn make_accounts_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("account_num", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
            Column::new("payload", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn create_test_db(prefix: &str) -> (TreeDb, tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{prefix}.treedb"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir, db_path)
}

fn reopen_db(path: &Path) -> TreeDb {
    TreeDb::open(path, 64).unwrap()
}

fn make_row(id: i64, acc: &str, bal: i64, payload: Vec<u8>) -> Row {
    Row::new(vec![
        Value::Int64(id),
        Value::String(acc.to_string()),
        Value::Int64(bal),
        Value::Bytes(payload),
    ])
}

fn assert_db_consistency(db: &mut TreeDb) {
    db.validate_freelist().expect("freelist must be valid");
    VacuumEngine::validate_db_vacuum_consistency(db).expect("vacuum consistency validation failed");
}

// =============================================================================
// Section 5 & 6: Transaction Recovery Matrix with Deterministic Failure Injection
// =============================================================================

#[test]
fn test_failure_injection_before_first_mutation() {
    let (mut db, _dir, path) = create_test_db("fail_before_first_mut");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::BeforeFirstMutation));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]));
        assert!(matches!(
            res,
            Err(Error::InjectedFault(
                StorageFailurePoint::BeforeFirstMutation
            ))
        ));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(
            tbl.get(&Value::Int64(2)).unwrap().is_none(),
            "Uncommitted row 2 must not be visible"
        );
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
    {
        let mut tbl = db2.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
}

#[test]
fn test_failure_injection_after_record_version_allocated() {
    let (mut db, _dir, path) = create_test_db("fail_after_ver_alloc");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterRecordVersionAllocated));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]));
        assert!(matches!(
            res,
            Err(Error::InjectedFault(
                StorageFailurePoint::AfterRecordVersionAllocated
            ))
        ));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
}

#[test]
fn test_failure_injection_after_record_payload_logged() {
    let (mut db, _dir, path) = create_test_db("fail_after_payload_logged");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterRecordPayloadLogged));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let _ = tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
}

#[test]
fn test_failure_injection_after_version_metadata_changed() {
    let (mut db, _dir, path) = create_test_db("fail_after_ver_meta");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterVersionMetadataChanged));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let _ = tbl.update(&make_row(1, "ACC-1", 200, vec![2]));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(100));
    }
}

#[test]
fn test_failure_injection_after_primary_index_modified() {
    let (mut db, _dir, path) = create_test_db("fail_after_primary_idx");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterPrimaryIndexModified));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]));
        assert!(matches!(
            res,
            Err(Error::InjectedFault(
                StorageFailurePoint::AfterPrimaryIndexModified
            ))
        ));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
}

#[test]
fn test_failure_injection_after_secondary_index_modified() {
    let (mut db, _dir, path) = create_test_db("fail_after_sec_idx");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterSecondaryIndexModified));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]));
        assert!(matches!(
            res,
            Err(Error::InjectedFault(
                StorageFailurePoint::AfterSecondaryIndexModified
            ))
        ));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
}

#[test]
fn test_failure_injection_during_overflow_page_alloc() {
    let (mut db, _dir, path) = create_test_db("fail_during_overflow");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let large_payload = vec![42u8; 8000];
    let mut t0 = db.begin().unwrap();
    t0.set_test_failure_point(Some(StorageFailurePoint::DuringOverflowPageAlloc));
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(1, "ACC-1", 100, large_payload));
        assert!(matches!(
            res,
            Err(Error::InjectedFault(
                StorageFailurePoint::DuringOverflowPageAlloc
            ))
        ));
    }
    std::mem::forget(t0);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_none());
    }
}

#[test]
fn test_failure_injection_during_record_slot_reclaim() {
    let (mut db, _dir, path) = create_test_db("fail_during_slot_reclaim");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::DuringRecordSlotReclaim));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let _ = tbl.delete(&Value::Int64(1));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
    }
}

#[test]
fn test_failure_injection_during_freelist_update() {
    let (mut db, _dir, path) = create_test_db("fail_during_freelist");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1; 7000]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    t1.set_test_failure_point(Some(StorageFailurePoint::DuringFreelistUpdate));
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let _ = tbl.delete(&Value::Int64(1));
    }
    std::mem::forget(t1);
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
    }
}

#[test]
fn test_failure_injection_before_commit_record_logged() {
    let (mut db, _dir, path) = create_test_db("fail_before_commit_logged");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]))
            .unwrap();
    }
    t1.set_test_failure_point(Some(StorageFailurePoint::BeforeCommitRecordLogged));
    let res = t1.commit();
    assert!(matches!(
        res,
        Err(Error::InjectedFault(
            StorageFailurePoint::BeforeCommitRecordLogged
        ))
    ));

    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(
            tbl.get(&Value::Int64(2)).unwrap().is_none(),
            "Txn without commit record must be rolled back"
        );
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
}

#[test]
fn test_failure_injection_after_commit_record_logged_before_flush() {
    let (mut db, _dir, path) = create_test_db("fail_after_logged_before_flush");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]))
            .unwrap();
    }
    t1.set_test_failure_point(Some(
        StorageFailurePoint::AfterCommitRecordLoggedBeforeFlush,
    ));
    let res = t1.commit();
    assert!(matches!(
        res,
        Err(Error::InjectedFault(
            StorageFailurePoint::AfterCommitRecordLoggedBeforeFlush
        ))
    ));

    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(
            tbl.get(&Value::Int64(2)).unwrap().is_none(),
            "Unflushed commit record is not durable"
        );
    }
}

#[test]
fn test_failure_injection_after_durable_commit_record_flush() {
    let (mut db, _dir, path) = create_test_db("fail_after_durable_flush");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1, 2, 3]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_row(2, "ACC-2", 200, vec![4, 5, 6]))
            .unwrap();
    }
    t1.set_test_failure_point(Some(StorageFailurePoint::AfterDurableCommitRecordFlush));
    let res = t1.commit();
    assert!(matches!(
        res,
        Err(Error::InjectedFault(
            StorageFailurePoint::AfterDurableCommitRecordFlush
        ))
    ));

    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        let r2 = tbl.get(&Value::Int64(2)).unwrap();
        assert!(
            r2.is_some(),
            "Durable commit record MUST survive recovery and be committed!"
        );
        let r2_val = r2.unwrap();
        assert_eq!(r2_val.values[1], Value::String("ACC-2".into()));
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);
    {
        let mut tbl = db2.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_some());
    }
}

#[test]
fn test_failure_injection_during_header_persistence() {
    let (mut db, _dir, path) = create_test_db("fail_during_header_persist");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    // Set failure point during header persistence on checkpoint
    db.set_test_failure_point(Some(StorageFailurePoint::DuringHeaderPersistence));
    let res = db.checkpoint();
    assert!(matches!(
        res,
        Err(Error::InjectedFault(
            StorageFailurePoint::DuringHeaderPersistence
        ))
    ));

    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
    }
}

#[test]
fn test_failure_injection_during_vacuum_phases() {
    let (mut db, _dir, path) = create_test_db("fail_during_vacuum");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(99, "BASELINE", 999, vec![99]))
            .unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1; 7000]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    t1.commit().unwrap();

    // 1. Vacuum with fault during index cleanup
    db.set_test_failure_point(Some(StorageFailurePoint::DuringVacuumIndexCleanup));
    let horizon = db.reclamation_horizon();
    {
        let mut tbl = db.open_table("accounts").unwrap();
        let _ = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon);
    }
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);

    // 2. Vacuum with fault during record reclamation
    db1.set_test_failure_point(Some(StorageFailurePoint::DuringVacuumRecordReclaim));
    let horizon1 = db1.reclamation_horizon();
    {
        let mut tbl = db1.open_table("accounts").unwrap();
        let _ = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon1);
    }
    drop(db1);

    let mut db2 = reopen_db(&path);
    assert_db_consistency(&mut db2);

    // 3. Complete normal vacuum pass cleanly
    let horizon2 = db2.reclamation_horizon();
    {
        let mut tbl = db2.open_table("accounts").unwrap();
        let _ = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon2);
    }
    assert_db_consistency(&mut db2);
}

// =============================================================================
// Section 7: Mixed-Operation Atomicity Tests
// =============================================================================

#[test]
fn test_insert_multiple_rows_then_abort() {
    let (mut db, _dir, path) = create_test_db("insert_mult_abort");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(10, "ACC-10", 1000, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        for i in 1..=5 {
            tbl.insert(&make_row(i, &format!("ACC-{i}"), i * 100, vec![i as u8]))
                .unwrap();
        }
    }
    t1.abort().unwrap();

    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(10)).unwrap().is_some());
        for i in 1..=5 {
            assert!(
                tbl.get(&Value::Int64(i)).unwrap().is_none(),
                "Aborted insert must not exist"
            );
        }
    }
    t2.commit().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(10)).unwrap().is_some());
        for i in 1..=5 {
            assert!(tbl.get(&Value::Int64(i)).unwrap().is_none());
        }
    }
}

#[test]
fn test_update_multiple_rows_then_validation_error() {
    let (mut db, _dir, path) = create_test_db("update_mult_validation_err");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
        tbl.insert(&make_row(2, "ACC-2", 200, vec![2])).unwrap();
        tbl.insert(&make_row(3, "ACC-3", 300, vec![3])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 150, vec![1, 1])).unwrap();
        tbl.update(&make_row(2, "ACC-2", 250, vec![2, 2])).unwrap();
        let err = tbl.update(&make_row(3, "ACC-1", 350, vec![3, 3]));
        assert!(err.is_err(), "Unique index violation must be detected");
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(
            r1.values[2],
            Value::Int64(100),
            "Row 1 must retain baseline value"
        );
        let r2 = tbl.get(&Value::Int64(2)).unwrap().unwrap();
        assert_eq!(
            r2.values[2],
            Value::Int64(200),
            "Row 2 must retain baseline value"
        );
        let r3 = tbl.get(&Value::Int64(3)).unwrap().unwrap();
        assert_eq!(
            r3.values[2],
            Value::Int64(300),
            "Row 3 must retain baseline value"
        );
    }
}

#[test]
fn test_delete_row_then_execution_error() {
    let (mut db, _dir, path) = create_test_db("del_row_exec_error");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
        let err = tbl.update(&make_row(999, "NON-EXISTENT", 0, vec![]));
        assert!(err.is_err());
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap();
        assert!(
            r1.is_some(),
            "Deleted row must be restored after aborted delete"
        );
    }
}

#[test]
fn test_change_secondary_index_key_then_abort() {
    let (mut db, _dir, path) = create_test_db("sec_idx_key_abort");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-ORIGINAL", 100, vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-MODIFIED", 200, vec![2]))
            .unwrap();
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[1], Value::String("ACC-ORIGINAL".into()));

        let res = tbl.index_scan("idx_acc", None, None).unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].values[1], Value::String("ACC-ORIGINAL".into()));

        let sql_res = db_reopened
            .execute_sql("SELECT balance FROM accounts WHERE account_num = 'ACC-ORIGINAL'")
            .unwrap();
        assert_eq!(sql_res.rows.len(), 1);
        assert_eq!(sql_res.rows[0].values[0], Value::Int64(100));

        let sql_mod = db_reopened
            .execute_sql("SELECT balance FROM accounts WHERE account_num = 'ACC-MODIFIED'")
            .unwrap();
        assert_eq!(
            sql_mod.rows.len(),
            0,
            "Aborted secondary index key must not be visible"
        );
    }
}

#[test]
fn test_update_overflow_record_then_abort() {
    let (mut db, _dir, path) = create_test_db("overflow_update_abort");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let payload_v1 = vec![111u8; 8000];
    let payload_v2 = vec![222u8; 8000];

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, payload_v1.clone()))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 200, payload_v2)).unwrap();
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(100));
        assert_eq!(r1.values[3], Value::Bytes(payload_v1));
    }
}

#[test]
fn test_delete_overflow_record_then_crash() {
    let (mut db, _dir, path) = create_test_db("overflow_del_crash");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let payload_v1 = vec![99u8; 9000];

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, payload_v1.clone()))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    std::mem::forget(t1);
    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[3], Value::Bytes(payload_v1));
    }
}

#[test]
fn test_primary_key_reuse_after_aborted_insert() {
    let (mut db, _dir, path) = create_test_db("pk_reuse_aborted_insert");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-ABORTED", 50, vec![1]))
            .unwrap();
    }
    t1.abort().unwrap();

    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-COMMITTED", 500, vec![2]))
            .unwrap();
    }
    t2.commit().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[1], Value::String("ACC-COMMITTED".into()));
        assert_eq!(r1.values[2], Value::Int64(500));
    }
}

#[test]
fn test_multiple_updates_to_same_row_before_commit() {
    let (mut db, _dir, path) = create_test_db("mult_updates_same_row");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![0])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 200, vec![1])).unwrap();
        tbl.update(&make_row(1, "ACC-1", 300, vec![2])).unwrap();
        tbl.update(&make_row(1, "ACC-1", 400, vec![3])).unwrap();
    }
    t1.commit().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(400));
        assert_eq!(r1.values[3], Value::Bytes(vec![3]));
    }
}

#[test]
fn test_vacuum_after_commit_and_after_abort() {
    let (mut db, _dir, path) = create_test_db("vacuum_commit_abort");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 200, vec![2])).unwrap();
    }
    t1.abort().unwrap();

    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 300, vec![3])).unwrap();
    }
    t2.commit().unwrap();

    let horizon = db.reclamation_horizon();
    {
        let mut tbl = db.open_table("accounts").unwrap();
        VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon).unwrap();
    }

    assert_db_consistency(&mut db);
    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(300));
    }
}

// =============================================================================
// Section 9: MVCC and Snapshot Recovery
// =============================================================================

#[test]
fn test_committed_update_with_older_active_snapshot() {
    let (mut db, _dir, path) = create_test_db("mvcc_older_snapshot");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let s1 = db.register_snapshot();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 500, vec![1, 1])).unwrap();
    }
    t1.commit().unwrap();

    // Verify: snapshot S1 sees 100, current transaction sees 500
    {
        let mut t_head = db.begin().unwrap();
        let mut tbl_head = t_head.open_table("accounts").unwrap();
        let r_head = tbl_head.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r_head.values[2], Value::Int64(500));
        drop(tbl_head);
        t_head.commit().unwrap();

        let mut t_s1 = db.begin().unwrap();
        t_s1.set_snapshot(s1);
        let mut tbl_s1 = t_s1.open_table("accounts").unwrap();
        let r_s1 = tbl_s1.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r_s1.values[2], Value::Int64(100));
        drop(tbl_s1);
        t_s1.commit().unwrap();
    }

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut t_head = db_reopened.begin().unwrap();
        let mut tbl_head = t_head.open_table("accounts").unwrap();
        let r_head = tbl_head.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r_head.values[2], Value::Int64(500));
    }
}

#[test]
fn test_committed_delete_followed_by_recovery() {
    let (mut db, _dir, path) = create_test_db("mvcc_delete_recovery");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    t1.commit().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_none());
    }
}

#[test]
fn test_commit_seq_non_regression_and_collision_freedom() {
    let (mut db, _dir, path) = create_test_db("commit_seq_non_regression");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut seqs = Vec::new();
    for i in 1..=5 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("accounts").unwrap();
            tbl.insert(&make_row(i, &format!("ACC-{i}"), i * 100, vec![i as u8]))
                .unwrap();
        }
        t.commit().unwrap();
        let seq = db.buffer_pool().durable_max_commit_seq();
        seqs.push(seq);
    }

    for window in seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "Commit sequence must be strictly increasing"
        );
    }

    let max_seq_before = *seqs.last().unwrap();
    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);

    let next_seq = db_reopened.current_commit_seq();
    assert!(
        next_seq.0 > max_seq_before,
        "Recovered commit sequence {next_seq:?} must not regress past {max_seq_before:?}"
    );

    let mut t_new = db_reopened.begin().unwrap();
    {
        let mut tbl = t_new.open_table("accounts").unwrap();
        tbl.insert(&make_row(99, "ACC-99", 9900, vec![99])).unwrap();
    }
    t_new.commit().unwrap();
    let final_seq = db_reopened.buffer_pool().durable_max_commit_seq();
    assert!(
        final_seq > max_seq_before,
        "New commit sequence must not collide"
    );
}

// =============================================================================
// Section 10: Independent Recovery Reference Model
// =============================================================================

#[derive(Debug, Clone, Default)]
struct ReferenceModel {
    committed_rows: BTreeMap<i64, (String, i64, Vec<u8>)>,
    staged_ops: Vec<StagedOp>,
}

#[derive(Debug, Clone)]
enum StagedOp {
    Insert(i64, String, i64, Vec<u8>),
    Update(i64, String, i64, Vec<u8>),
    Delete(i64),
}

impl ReferenceModel {
    fn new() -> Self {
        Self::default()
    }

    fn stage_insert(&mut self, id: i64, acc: String, bal: i64, payload: Vec<u8>) {
        self.staged_ops
            .push(StagedOp::Insert(id, acc, bal, payload));
    }

    fn stage_update(&mut self, id: i64, acc: String, bal: i64, payload: Vec<u8>) {
        self.staged_ops
            .push(StagedOp::Update(id, acc, bal, payload));
    }

    fn stage_delete(&mut self, id: i64) {
        self.staged_ops.push(StagedOp::Delete(id));
    }

    fn commit(&mut self) {
        for op in self.staged_ops.drain(..) {
            match op {
                StagedOp::Insert(id, acc, bal, payload) => {
                    self.committed_rows.insert(id, (acc, bal, payload));
                }
                StagedOp::Update(id, acc, bal, payload) => {
                    self.committed_rows.insert(id, (acc, bal, payload));
                }
                StagedOp::Delete(id) => {
                    self.committed_rows.remove(&id);
                }
            }
        }
    }

    fn abort(&mut self) {
        self.staged_ops.clear();
    }
}

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0xdeadbeef } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn gen_range(&mut self, low: u64, high: u64) -> u64 {
        assert!(high > low);
        low + (self.next_u64() % (high - low))
    }
}

#[test]
fn test_differential_reference_model_fixed_seeds() {
    let test_seeds: [u64; 4] = [42, 1337, 2026, 9999];

    for &seed in &test_seeds {
        let (mut db, _dir, path) = create_test_db(&format!("ref_model_{seed}"));
        db.create_table("accounts", make_accounts_schema()).unwrap();
        db.create_index("accounts", "idx_acc", "account_num", false)
            .unwrap();

        let mut model = ReferenceModel::new();
        let mut rng = SimpleRng::new(seed);

        for round in 0..25 {
            let mut txn = db.begin().unwrap();
            let num_ops = rng.gen_range(1, 6);
            let mut failed = false;

            for _ in 0..num_ops {
                let op_type = rng.gen_range(0, 3);
                let id = rng.gen_range(1, 15) as i64;
                let bal = rng.gen_range(10, 1000) as i64;
                let acc = format!("ACC-{id}-{round}");
                let payload = vec![(round % 250) as u8; (rng.gen_range(10, 100)) as usize];

                let mut tbl = txn.open_table("accounts").unwrap();
                match op_type {
                    0 => {
                        let row = make_row(id, &acc, bal, payload.clone());
                        if tbl.insert(&row).is_ok() {
                            model.stage_insert(id, acc, bal, payload);
                        } else {
                            failed = true;
                            break;
                        }
                    }
                    1 => {
                        let row = make_row(id, &acc, bal, payload.clone());
                        if tbl.update(&row).is_ok() {
                            model.stage_update(id, acc, bal, payload);
                        } else {
                            failed = true;
                            break;
                        }
                    }
                    _ => {
                        if tbl.delete(&Value::Int64(id)).is_ok() {
                            model.stage_delete(id);
                        } else {
                            failed = true;
                            break;
                        }
                    }
                }
            }

            let should_commit = !failed && (rng.gen_range(0, 10) > 2);
            if should_commit {
                if txn.commit().is_ok() {
                    model.commit();
                } else {
                    model.abort();
                }
            } else {
                let _ = txn.abort();
                model.abort();
            }

            if round % 8 == 7 {
                drop(db);
                db = reopen_db(&path);
                assert_db_consistency(&mut db);
            }
        }

        drop(db);
        let mut final_db = reopen_db(&path);
        assert_db_consistency(&mut final_db);

        {
            let mut tbl = final_db.open_table("accounts").unwrap();
            for id in 1..15 {
                let expected = model.committed_rows.get(&id);
                let actual = tbl.get(&Value::Int64(id)).unwrap();

                match (expected, actual) {
                    (Some((exp_acc, exp_bal, exp_payload)), Some(act_row)) => {
                        assert_eq!(
                            act_row.values[0],
                            Value::Int64(id),
                            "Seed {seed}: id mismatch"
                        );
                        assert_eq!(
                            act_row.values[1],
                            Value::String(exp_acc.clone()),
                            "Seed {seed}: acc mismatch"
                        );
                        assert_eq!(
                            act_row.values[2],
                            Value::Int64(*exp_bal),
                            "Seed {seed}: balance mismatch"
                        );
                        assert_eq!(
                            act_row.values[3],
                            Value::Bytes(exp_payload.clone()),
                            "Seed {seed}: payload mismatch"
                        );
                    }
                    (None, None) => {}
                    (Some(_), None) => {
                        panic!("Seed {seed}: Expected row id {id} to exist, but was None");
                    }
                    (None, Some(act_row)) => {
                        panic!(
                            "Seed {seed}: Expected row id {id} to be absent, but found {act_row:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn test_modify_covering_index_payload_then_abort() {
    let (mut db, _dir, path) = create_test_db("cov_idx_abort");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.execute_statement("CREATE INDEX idx_cov ON accounts (account_num) INCLUDE (balance);")
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 999, vec![2])).unwrap();
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    let res = db_reopened
        .execute_sql("SELECT balance FROM accounts WHERE account_num = 'ACC-1'")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(
        res.rows[0].values[0],
        Value::Int64(100),
        "Covering index payload must retain baseline value"
    );
}

#[test]
fn test_insert_unique_index_key_fail_before_commit() {
    let (mut db, _dir, path) = create_test_db("uniq_idx_fail_commit");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-DUPE", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        let res = tbl.insert(&make_row(2, "ACC-DUPE", 200, vec![2]));
        assert!(res.is_err(), "Duplicate unique key must be rejected");
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(tbl.get(&Value::Int64(2)).unwrap().is_none());
    }
}

#[test]
fn test_conflicting_write_after_earlier_operations_succeeded() {
    let (mut db, _dir, path) = create_test_db("conflict_after_earlier_ops");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_row(2, "ACC-2", 200, vec![2])).unwrap();
        tbl.insert(&make_row(3, "ACC-3", 300, vec![3])).unwrap();
        let res = tbl.insert(&make_row(4, "ACC-1", 400, vec![4]));
        assert!(res.is_err(), "Duplicate unique secondary key must conflict");
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        assert!(tbl.get(&Value::Int64(1)).unwrap().is_some());
        assert!(
            tbl.get(&Value::Int64(2)).unwrap().is_none(),
            "Row 2 from aborted txn must not exist"
        );
        assert!(
            tbl.get(&Value::Int64(3)).unwrap().is_none(),
            "Row 3 from aborted txn must not exist"
        );
        assert!(
            tbl.get(&Value::Int64(4)).unwrap().is_none(),
            "Row 4 must not exist"
        );
    }
}

#[test]
fn test_aborted_update_with_newer_snapshot() {
    let (mut db, _dir, path) = create_test_db("mvcc_aborted_update_snapshot");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 999, vec![9])).unwrap();
    }
    t1.abort().unwrap();

    let s_newer = db.register_snapshot();
    let mut t2 = db.begin().unwrap();
    t2.set_snapshot(s_newer);
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(
            r1.values[2],
            Value::Int64(100),
            "Newer snapshot must see committed baseline, not aborted update"
        );
    }
    t2.commit().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(100));
    }
}

#[test]
fn test_aborted_delete_followed_by_recovery() {
    let (mut db, _dir, path) = create_test_db("mvcc_aborted_delete_recovery");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    t1.abort().unwrap();

    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(
            r1.values[2],
            Value::Int64(100),
            "Aborted delete must leave row visible after recovery"
        );
    }
}

#[test]
fn test_historical_versions_reclamation_horizon() {
    let (mut db, _dir, path) = create_test_db("historical_horizon");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1])).unwrap();
    }
    t0.commit().unwrap();

    // S1 pins version 1
    let s1 = db.register_snapshot();

    // T2 updates row 1 to 200
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 200, vec![2])).unwrap();
    }
    t1.commit().unwrap();

    // T3 updates row 1 to 300
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.update(&make_row(1, "ACC-1", 300, vec![3])).unwrap();
    }
    t2.commit().unwrap();

    // While S1 is active, vacuum cannot prune version 1
    let horizon_with_s1 = db.reclamation_horizon();
    {
        let mut tbl = db.open_table("accounts").unwrap();
        let res = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon_with_s1)
            .unwrap();
        assert_eq!(
            res.versions_reclaimed, 0,
            "No versions should be reclaimed while S1 is active"
        );
    }

    // Release snapshot S1
    db.release_snapshot(s1.snapshot_id);

    // Now vacuum can prune historical versions
    let horizon_after = db.reclamation_horizon();
    {
        let mut tbl = db.open_table("accounts").unwrap();
        let res = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon_after)
            .unwrap();
        assert!(
            res.versions_reclaimed > 0,
            "Older versions should be reclaimed once S1 is released"
        );
    }

    assert_db_consistency(&mut db);
    drop(db);

    let mut db_reopened = reopen_db(&path);
    assert_db_consistency(&mut db_reopened);
    {
        let mut tbl = db_reopened.open_table("accounts").unwrap();
        let r1 = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(r1.values[2], Value::Int64(300));
    }
}

#[test]
fn test_failure_injection_during_vacuum_freelist_update() {
    let (mut db, _dir, path) = create_test_db("fail_vac_freelist");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_row(99, "BASELINE", 999, vec![99]))
            .unwrap();
        tbl.insert(&make_row(1, "ACC-1", 100, vec![1; 100]))
            .unwrap();
    }
    t0.commit().unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(1)).unwrap();
    }
    t1.commit().unwrap();

    db.set_test_failure_point(Some(StorageFailurePoint::DuringVacuumFreelistUpdate));
    let horizon = db.reclamation_horizon();
    {
        let mut tbl = db.open_table("accounts").unwrap();
        let _ = VacuumEngine::vacuum_table(&mut tbl, &VacuumConfig::unlimited(), horizon);
    }
    drop(db);

    let mut db1 = reopen_db(&path);
    assert_db_consistency(&mut db1);
}
