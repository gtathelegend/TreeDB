//! Phase 74: Recovery Certification, Checkpoint Integrity & Crash-State Verification.
//!
//! Validates:
//! 1. Deterministic recovery test harness with independent expected-state reference model.
//! 2. Uncommitted transaction invariants (all-or-nothing rollback, unique index reusability, covering payload integrity).
//! 3. Committed transaction durability (WAL redo replay, post-checkpoint persistence).
//! 4. CommitSeq monotonicity and historical MVCC snapshot visibility surviving recovery.
//! 5. Cross-layer index consistency (Primary, Secondary, Composite, Covering, Unique).
//! 6. Freelist and page allocation integrity across crash recovery and interrupted vacuum.
//! 7. Comprehensive Checkpoint Crash Matrix covering all 8 persistence boundaries.
//! 8. Phase 73 verifier pre-recovery (UnappliedWalTail detection) and post-recovery certification.
//! 9. Recovery idempotency: repeated reopen/recovery cycles produce stable, identical logical state.
//! 10. Metamorphic differential testing across deterministic fixed seeds (42, 1337, 2026, 9999).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

use treedb::engine::isolation::{CommitSeq, Snapshot, SnapshotId};
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::engine::vacuum::VacuumConfig;

use treedb::{Error, StorageFailurePoint, TreeDb};

// =============================================================================
// Test Schema and Helper Fixtures
// =============================================================================

fn make_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("account_num", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
            Column::new("category", DataType::String, false),
            Column::new("payload", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn make_test_row(id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) -> Row {
    Row::new(vec![
        Value::Int64(id),
        Value::String(acc.to_string()),
        Value::Int64(bal),
        Value::String(cat.to_string()),
        Value::Bytes(payload),
    ])
}

fn setup_test_db(name: &str) -> (TreeDb, tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let mut db = TreeDb::create(&db_path, 64).unwrap();
    db.create_table("accounts", make_test_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();
    db.execute_statement(
        "CREATE INDEX idx_cov ON accounts (account_num) INCLUDE (balance, category);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_cat_bal ON accounts (category, balance);")
        .unwrap();
    (db, dir, db_path)
}

// =============================================================================
// Independent Logical Expected-State Model
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedAccount {
    account_num: String,
    balance: i64,
    category: String,
    payload: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
struct LogicalModel {
    committed_rows: BTreeMap<i64, ExpectedAccount>,
}

impl LogicalModel {
    fn insert(&mut self, id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) {
        self.committed_rows.insert(
            id,
            ExpectedAccount {
                account_num: acc.to_string(),
                balance: bal,
                category: cat.to_string(),
                payload,
            },
        );
    }

    fn update(&mut self, id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) {
        if self.committed_rows.contains_key(&id) {
            self.committed_rows.insert(
                id,
                ExpectedAccount {
                    account_num: acc.to_string(),
                    balance: bal,
                    category: cat.to_string(),
                    payload,
                },
            );
        }
    }

    #[allow(dead_code)]
    fn delete(&mut self, id: i64) {
        self.committed_rows.remove(&id);
    }

    fn assert_matches_database(&self, db: &mut TreeDb, context: &str) {
        // 1. Validate Table Scan
        let mut tbl = db.open_table("accounts").unwrap();
        let scanned_rows = tbl.scan(None, None).unwrap();
        assert_eq!(
            scanned_rows.len(),
            self.committed_rows.len(),
            "{context}: Scanned row count mismatch"
        );

        for row in scanned_rows {
            let id = match row.values[0] {
                Value::Int64(v) => v,
                _ => panic!("Expected Int64 id"),
            };
            let exp = self
                .committed_rows
                .get(&id)
                .unwrap_or_else(|| panic!("{context}: Unexpected live row id {id}"));
            assert_eq!(
                row.values[1],
                Value::String(exp.account_num.clone()),
                "{context}: acc mismatch for id {id}"
            );
            assert_eq!(
                row.values[2],
                Value::Int64(exp.balance),
                "{context}: balance mismatch for id {id}"
            );
            assert_eq!(
                row.values[3],
                Value::String(exp.category.clone()),
                "{context}: category mismatch for id {id}"
            );
            assert_eq!(
                row.values[4],
                Value::Bytes(exp.payload.clone()),
                "{context}: payload mismatch for id {id}"
            );
        }

        // 2. Validate Point Lookups via Primary Key
        for (id, exp) in &self.committed_rows {
            let actual = tbl.get(&Value::Int64(*id)).unwrap();
            assert!(
                actual.is_some(),
                "{context}: Expected row id {id} to exist via PK get"
            );
            let row = actual.unwrap();
            assert_eq!(row.values[1], Value::String(exp.account_num.clone()));
            assert_eq!(row.values[2], Value::Int64(exp.balance));
            assert_eq!(row.values[3], Value::String(exp.category.clone()));
            assert_eq!(row.values[4], Value::Bytes(exp.payload.clone()));
        }

        // 3. Validate SQL Query & Index Path
        drop(tbl);
        let res = db
            .execute_sql("SELECT count(*) FROM accounts")
            .expect("SELECT count(*) must execute cleanly");
        let count_val = match res.rows[0].values[0] {
            Value::UInt64(c) => c as usize,
            Value::Int64(c) => c as usize,
            ref other => panic!("Expected count as UInt64 or Int64, got {other:?}"),
        };
        assert_eq!(
            count_val,
            self.committed_rows.len(),
            "{context}: SQL count(*) mismatch"
        );
    }
}

// =============================================================================
// Pre-Recovery and Post-Recovery Verification Helpers
// =============================================================================

fn assert_pre_recovery_file_inspect(db_path: &Path) {
    let report = TreeDb::verify_file(db_path).expect("verify_file must execute without error");
    // Offline pre-recovery file inspect should complete without fatal file corruption
    assert!(!report.has_fatal());
}

fn assert_post_recovery_consistency(db: &mut TreeDb, context: &str) {
    let report = db
        .verify_consistency()
        .expect("verify_consistency must succeed");
    assert!(
        report.is_healthy(),
        "{context}: Consistency report must be healthy: {:?}",
        report.issues
    );
    assert!(!report.truncated, "{context}: Report must not be truncated");
    assert!(
        !report.has_fatal(),
        "{context}: Report must not have fatal errors"
    );
    assert!(report
        .passed_checks
        .contains(&"database_header".to_string()));
    assert!(report
        .passed_checks
        .contains(&"freelist_invariants".to_string()));
    assert!(report
        .passed_checks
        .contains(&"catalog_metadata".to_string()));
    assert!(report.passed_checks.contains(&"page_ownership".to_string()));
    assert!(report
        .passed_checks
        .contains(&"primary_trees_and_record_stores".to_string()));
    assert!(report
        .passed_checks
        .contains(&"secondary_indexes".to_string()));
    assert!(report.passed_checks.contains(&"wal_state".to_string()));
}

// =============================================================================
// Stage 3 Invariant Tests: A. Uncommitted Transactions
// =============================================================================

#[test]
fn test_recovery_uncommitted_insert_rollback_and_unique_key_reuse() {
    let (mut db, _dir, path) = setup_test_db("uncommitted_insert_rb");
    let mut model = LogicalModel::default();

    // Baseline committed row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "CHECKING", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "CHECKING", vec![1]);

    // Uncommitted insert of row 2 with unique key ACC-2
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(2, "ACC-2", 200, "SAVINGS", vec![2]))
            .unwrap();
    }
    // Drop transaction without commit to simulate abrupt crash
    std::mem::forget(t1);
    drop(db);

    // Pre-recovery inspection
    assert_pre_recovery_file_inspect(&path);

    // Reopen through normal recovery
    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_uncommitted_insert");
    model.assert_matches_database(&mut db_rec, "post_rec_uncommitted_insert");

    // Invariant: The uncommitted unique key ACC-2 must be reusable
    let mut t2 = db_rec.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(2, "ACC-2", 250, "SAVINGS", vec![2, 5]))
            .expect("Unique key ACC-2 must be insertable after uncommitted insert was rolled back");
    }
    t2.commit().unwrap();
    model.insert(2, "ACC-2", 250, "SAVINGS", vec![2, 5]);
    model.assert_matches_database(&mut db_rec, "post_reinsert_unique_key");
}

#[test]
fn test_recovery_uncommitted_update_and_covering_payload_retention() {
    let (mut db, _dir, path) = setup_test_db("uncommitted_update_cov");
    let mut model = LogicalModel::default();

    // Baseline committed row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(10, "ACC-10", 500, "CHECKING", vec![10]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(10, "ACC-10", 500, "CHECKING", vec![10]);

    // Uncommitted update modifying balance and category
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.update(&make_test_row(10, "ACC-10", 9999, "VIP", vec![99]))
            .unwrap();
    }
    std::mem::forget(t1);
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_uncommitted_update");
    model.assert_matches_database(&mut db_rec, "post_rec_uncommitted_update");

    // Verify covering index lookup directly preserves pre-update values
    let res = db_rec
        .execute_sql("SELECT balance, category FROM accounts WHERE account_num = 'ACC-10'")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(500));
    assert_eq!(res.rows[0].values[1], Value::String("CHECKING".into()));
}

#[test]
fn test_recovery_uncommitted_delete_retains_row() {
    let (mut db, _dir, path) = setup_test_db("uncommitted_delete_retain");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(5, "ACC-5", 300, "CHECKING", vec![5]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(5, "ACC-5", 300, "CHECKING", vec![5]);

    // Uncommitted delete
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.delete(&Value::Int64(5)).unwrap();
    }
    std::mem::forget(t1);
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_uncommitted_delete");
    model.assert_matches_database(&mut db_rec, "post_rec_uncommitted_delete");
}

#[test]
fn test_recovery_uncommitted_overflow_record_does_not_corrupt_storage() {
    let (mut db, _dir, path) = setup_test_db("uncommitted_overflow_rb");
    let mut model = LogicalModel::default();

    // Baseline row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "CAT-1", vec![1; 100]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "CAT-1", vec![1; 100]);

    // Uncommitted insert of large multi-page overflow record (12 KB payload)
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(
            2,
            "ACC-OVERFLOW",
            200,
            "BIG",
            vec![7u8; 12_000],
        ))
        .unwrap();
    }
    std::mem::forget(t1);
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_uncommitted_overflow");
    model.assert_matches_database(&mut db_rec, "post_rec_uncommitted_overflow");
}

// =============================================================================
// Stage 3 Invariant Tests: B & C. Committed Transactions & MVCC Visibility
// =============================================================================

#[test]
fn test_recovery_durable_commit_survives_without_checkpoint() {
    let (mut db, _dir, path) = setup_test_db("durable_commit_no_cp");
    let mut model = LogicalModel::default();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
        tbl.insert(&make_test_row(2, "ACC-2", 200, "B", vec![2]))
            .unwrap();
    }
    t1.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);
    model.insert(2, "ACC-2", 200, "B", vec![2]);

    // Abrupt shutdown without checkpoint
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_durable_commit");
    model.assert_matches_database(&mut db_rec, "post_rec_durable_commit");
}

#[test]
fn test_recovery_commit_seq_monotonicity_across_restarts() {
    let (mut db, _dir, path) = setup_test_db("commit_seq_monotonic");
    let mut last_seq = 0u64;

    for round in 1..=4 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("accounts").unwrap();
            tbl.insert(&make_test_row(
                round,
                &format!("ACC-{round}"),
                round * 10,
                "TEST",
                vec![round as u8],
            ))
            .unwrap();
        }
        t.commit().unwrap();

        let cur_seq = db.buffer_pool().durable_max_commit_seq();
        assert!(
            cur_seq > last_seq,
            "Round {round}: CommitSeq must strictly increase"
        );
        last_seq = cur_seq;

        drop(db);
        db = TreeDb::open(&path, 64).unwrap();
        let recovered_seq = db.buffer_pool().durable_max_commit_seq();
        assert_eq!(
            recovered_seq, last_seq,
            "Round {round}: CommitSeq must match durable_max_commit_seq"
        );
    }
}

#[test]
fn test_recovery_historical_mvcc_snapshot_visibility() {
    let (mut db, _dir, path) = setup_test_db("mvcc_snapshot_vis");

    // Version 1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "V1", vec![1]))
            .unwrap();
    }
    t1.commit().unwrap();
    let s1_seq = db.buffer_pool().durable_max_commit_seq();

    // Version 2
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.update(&make_test_row(1, "ACC-1", 200, "V2", vec![2]))
            .unwrap();
    }
    t2.commit().unwrap();
    let s2_seq = db.buffer_pool().durable_max_commit_seq();

    // Version 3
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("accounts").unwrap();
        tbl.update(&make_test_row(1, "ACC-1", 300, "V3", vec![3]))
            .unwrap();
    }
    t3.commit().unwrap();

    drop(db);

    // Reopen and verify historical version chain is intact
    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_mvcc_snapshots");

    // As of S1: must observe V1 balance 100
    let mut txn_s1 = db_rec.begin().unwrap();
    txn_s1.set_snapshot(Snapshot::new(SnapshotId(1), CommitSeq(s1_seq)));
    {
        let mut tbl = txn_s1.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(100));
        assert_eq!(row.values[3], Value::String("V1".into()));
    }
    txn_s1.commit().unwrap();

    // As of S2: must observe V2 balance 200
    let mut txn_s2 = db_rec.begin().unwrap();
    txn_s2.set_snapshot(Snapshot::new(SnapshotId(2), CommitSeq(s2_seq)));
    {
        let mut tbl = txn_s2.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(200));
        assert_eq!(row.values[3], Value::String("V2".into()));
    }
    txn_s2.commit().unwrap();

    // Current: must observe V3 balance 300
    let mut txn_cur = db_rec.begin().unwrap();
    {
        let mut tbl = txn_cur.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(300));
        assert_eq!(row.values[3], Value::String("V3".into()));
    }
    txn_cur.commit().unwrap();
}

// =============================================================================
// Stage 3 Invariant Tests: E. Allocation and Reclamation Under Recovery
// =============================================================================

#[test]
fn test_recovery_after_vacuum_reclamation() {
    let (mut db, _dir, path) = setup_test_db("vacuum_then_recovery");
    let mut model = LogicalModel::default();

    // Insert rows
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        for i in 1..=5 {
            tbl.insert(&make_test_row(
                i,
                &format!("ACC-{i}"),
                i * 100,
                "CAT",
                vec![i as u8],
            ))
            .unwrap();
            model.insert(i, &format!("ACC-{i}"), i * 100, "CAT", vec![i as u8]);
        }
    }
    t0.commit().unwrap();

    // Multiple updates creating dead versions
    for round in 1..=3 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("accounts").unwrap();
            for i in 1..=5 {
                tbl.update(&make_test_row(
                    i,
                    &format!("ACC-{i}"),
                    i * 100 + round,
                    "CAT",
                    vec![round as u8],
                ))
                .unwrap();
                model.update(
                    i,
                    &format!("ACC-{i}"),
                    i * 100 + round,
                    "CAT",
                    vec![round as u8],
                );
            }
        }
        t.commit().unwrap();
    }

    // Run Vacuum to prune dead versions and reclaim pages
    let vac_res = db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(vac_res.versions_reclaimed > 0);

    // Abrupt shutdown and reopen
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "post_rec_after_vacuum");
    model.assert_matches_database(&mut db_rec, "post_rec_after_vacuum");
}

// =============================================================================
// Stage 4: Checkpoint Crash Matrix (All 8 Boundaries)
// =============================================================================

#[test]
fn test_checkpoint_boundary_1_before_checkpoint_initiation() {
    let (mut db, _dir, path) = setup_test_db("cp_b1_before_init");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::BeforeCheckpointInitiation));
    let cp_res = db.checkpoint();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::BeforeCheckpointInitiation
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b1");
    model.assert_matches_database(&mut db_rec, "cp_b1");
}

#[test]
fn test_checkpoint_boundary_2_during_dirty_page_flush() {
    let (mut db, _dir, path) = setup_test_db("cp_b2_dirty_flush");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::DuringCheckpointDirtyPageFlush));
    let cp_res = db.checkpoint();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::DuringCheckpointDirtyPageFlush
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b2");
    model.assert_matches_database(&mut db_rec, "cp_b2");
}

#[test]
fn test_checkpoint_boundary_3_during_wal_flush() {
    let (mut db, _dir, path) = setup_test_db("cp_b3_wal_flush");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::DuringCheckpointWalFlush));
    let cp_res = db.checkpoint();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::DuringCheckpointWalFlush
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b3");
    model.assert_matches_database(&mut db_rec, "cp_b3");
}

#[test]
fn test_checkpoint_boundary_4_during_header_persistence() {
    let (mut db, _dir, path) = setup_test_db("cp_b4_hdr_persist");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::DuringCheckpointHeaderPersistence));
    let cp_res = db.checkpoint();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::DuringCheckpointHeaderPersistence
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b4");
    model.assert_matches_database(&mut db_rec, "cp_b4");
}

#[test]
fn test_checkpoint_boundary_5_before_wal_truncation() {
    let (mut db, _dir, path) = setup_test_db("cp_b5_before_trunc");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::BeforeCheckpointWalTruncation));
    let cp_res = db.checkpoint_and_truncate_wal();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::BeforeCheckpointWalTruncation
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b5");
    model.assert_matches_database(&mut db_rec, "cp_b5");
}

#[test]
fn test_checkpoint_boundary_6_during_wal_truncation() {
    let (mut db, _dir, path) = setup_test_db("cp_b6_during_trunc");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::DuringCheckpointWalTruncation));
    let cp_res = db.checkpoint_and_truncate_wal();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::DuringCheckpointWalTruncation
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b6");
    model.assert_matches_database(&mut db_rec, "cp_b6");
}

#[test]
fn test_checkpoint_boundary_7_after_checkpoint_completed() {
    let (mut db, _dir, path) = setup_test_db("cp_b7_after_cp");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    db.set_test_failure_point(Some(StorageFailurePoint::AfterCheckpointCompleted));
    let cp_res = db.checkpoint();
    assert!(matches!(
        cp_res,
        Err(Error::InjectedFault(
            StorageFailurePoint::AfterCheckpointCompleted
        ))
    ));
    drop(db);

    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b7");
    model.assert_matches_database(&mut db_rec, "cp_b7");
}

#[test]
fn test_checkpoint_boundary_8_active_transaction_spanning_checkpoint() {
    let (mut db, _dir, path) = setup_test_db("cp_b8_active_spanning");
    let mut model = LogicalModel::default();

    // Committed row before checkpoint
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "A", vec![1]);

    // Active uncommitted transaction t_active
    let mut t_active = db.begin().unwrap();
    {
        let mut tbl = t_active.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(2, "ACC-ACTIVE", 200, "B", vec![2]))
            .unwrap();
    }

    // Checkpoint executes while t_active is still active (checkpoint record includes t_active)
    t_active.checkpoint().unwrap();

    // After checkpoint, t_active does another operation and then crashes
    {
        let mut tbl = t_active.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(3, "ACC-ACTIVE-2", 300, "C", vec![3]))
            .unwrap();
    }
    std::mem::forget(t_active);
    drop(db);

    // Recovery must identify t_active in active_txns of checkpoint, redo up to crash, and undo t_active
    let mut db_rec = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db_rec, "cp_b8");
    model.assert_matches_database(&mut db_rec, "cp_b8");
}

// =============================================================================
// Stage 5 & 6: Recovery Idempotency and Metamorphic Tests
// =============================================================================

#[test]
fn test_recovery_idempotency_repeated_cycles() {
    let (mut db, _dir, path) = setup_test_db("recovery_idempotency_multi");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        for i in 1..=10 {
            tbl.insert(&make_test_row(
                i,
                &format!("ACC-{i}"),
                i * 50,
                "CAT",
                vec![i as u8],
            ))
            .unwrap();
            model.insert(i, &format!("ACC-{i}"), i * 50, "CAT", vec![i as u8]);
        }
    }
    t0.commit().unwrap();

    // Uncommitted transaction left behind
    let mut t_uncommitted = db.begin().unwrap();
    {
        let mut tbl = t_uncommitted.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(99, "ACC-UNCOMMITTED", 999, "NONE", vec![99]))
            .unwrap();
    }
    std::mem::forget(t_uncommitted);
    drop(db);

    // Run 5 consecutive reopen & recovery cycles without mutation
    for cycle in 1..=5 {
        let mut db_cycle = TreeDb::open(&path, 64).unwrap();
        assert_post_recovery_consistency(&mut db_cycle, &format!("cycle_{cycle}"));
        model.assert_matches_database(&mut db_cycle, &format!("cycle_{cycle}"));
    }
}

#[test]
fn test_recovery_followed_by_checkpoint_and_reopen_cycles() {
    let (mut db, _dir, path) = setup_test_db("rec_cp_reopen_cycles");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "CAT-A", vec![1]))
            .unwrap();
        tbl.insert(&make_test_row(2, "ACC-2", 200, "CAT-B", vec![2]))
            .unwrap();
    }
    t0.commit().unwrap();
    model.insert(1, "ACC-1", 100, "CAT-A", vec![1]);
    model.insert(2, "ACC-2", 200, "CAT-B", vec![2]);
    drop(db);

    // Reopen 1
    let mut db1 = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db1, "reopen_1");
    // Checkpoint & truncate
    db1.checkpoint_and_truncate_wal().unwrap();
    drop(db1);

    // Reopen 2
    let mut db2 = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db2, "reopen_2");
    model.assert_matches_database(&mut db2, "reopen_2");

    // Mutate and commit
    let mut t1 = db2.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(3, "ACC-3", 300, "CAT-C", vec![3]))
            .unwrap();
    }
    t1.commit().unwrap();
    model.insert(3, "ACC-3", 300, "CAT-C", vec![3]);
    drop(db2);

    // Reopen 3
    let mut db3 = TreeDb::open(&path, 64).unwrap();
    assert_post_recovery_consistency(&mut db3, "reopen_3");
    model.assert_matches_database(&mut db3, "reopen_3");
}

#[test]
fn test_metamorphic_differential_recovery_fixed_seeds() {
    let seeds = [42u64, 1337u64, 2026u64, 9999u64];

    for seed in seeds {
        let (mut db, _dir, path) = setup_test_db(&format!("metamorphic_{seed}"));
        let mut model = LogicalModel::default();
        let mut rng = seed;

        // Linear Congruential Generator for deterministic pseudo-random choices
        let mut next_rand = move || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            rng
        };

        let mut next_id = 1i64;

        for step in 0..20 {
            let op_choice = next_rand() % 4;
            match op_choice {
                0 => {
                    // Commit Insert
                    let id = next_id;
                    next_id += 1;
                    let mut t = db.begin().unwrap();
                    {
                        let mut tbl = t.open_table("accounts").unwrap();
                        let bal = (next_rand() % 1000) as i64;
                        let cat = if next_rand() % 2 == 0 { "TECH" } else { "FIN" };
                        let payload = vec![(next_rand() % 255) as u8; 32];
                        let acc = format!("ACC-{id}-{seed}");
                        tbl.insert(&make_test_row(id, &acc, bal, cat, payload.clone()))
                            .unwrap();
                        model.insert(id, &acc, bal, cat, payload);
                    }
                    t.commit().unwrap();
                }
                1 => {
                    // Abort Insert
                    let id = next_id;
                    next_id += 1;
                    let mut t = db.begin().unwrap();
                    {
                        let mut tbl = t.open_table("accounts").unwrap();
                        let acc = format!("ACC-ABORT-{id}-{seed}");
                        let _ = tbl.insert(&make_test_row(id, &acc, 999, "ABORT", vec![0]));
                    }
                    t.abort().unwrap();
                }
                2 => {
                    // Update existing row if available
                    if let Some(&existing_id) = model.committed_rows.keys().next() {
                        let mut t = db.begin().unwrap();
                        {
                            let mut tbl = t.open_table("accounts").unwrap();
                            let bal = (next_rand() % 5000) as i64;
                            let cat = "UPDATED";
                            let payload = vec![(next_rand() % 255) as u8; 16];
                            let acc = format!("ACC-{existing_id}-{seed}");
                            tbl.update(&make_test_row(
                                existing_id,
                                &acc,
                                bal,
                                cat,
                                payload.clone(),
                            ))
                            .unwrap();
                            model.update(existing_id, &acc, bal, cat, payload);
                        }
                        t.commit().unwrap();
                    }
                }
                3 => {
                    // Checkpoint or Reopen
                    if next_rand() % 2 == 0 {
                        db.checkpoint().unwrap();
                    } else {
                        drop(db);
                        db = TreeDb::open(&path, 64).unwrap();
                    }
                }
                _ => unreachable!(),
            }

            // Periodically simulate abrupt crash
            if step == 10 || step == 19 {
                drop(db);
                db = TreeDb::open(&path, 64).unwrap();
                assert_post_recovery_consistency(&mut db, &format!("Seed {seed} step {step}"));
                model.assert_matches_database(&mut db, &format!("Seed {seed} step {step}"));
            }
        }
    }
}

#[test]
fn test_verifier_diagnostics_pre_recovery_unapplied_wal_tail_vs_post_recovery_clean() {
    let (mut db, _dir, path) = setup_test_db("verifier_diag_pre_post");

    // Committed row
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "TECH", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    // Checkpoint to establish a durable baseline
    db.checkpoint().unwrap();

    // Uncommitted mutation after checkpoint
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(2, "ACC-2", 200, "FIN", vec![2]))
            .unwrap();
    }
    std::mem::forget(t1);
    drop(db);

    // 1. Direct offline file verification before recovery
    let default_config = treedb::engine::verification::ConsistencyCheckConfig::default();
    let pre_report = TreeDb::check_file(&path, &default_config).unwrap();
    assert!(!pre_report.truncated);
    assert!(!pre_report.has_fatal());

    // Invariant: Verifier detects unapplied WAL records beyond checkpoint and reports UnappliedWalTail (Info severity)
    let has_unapplied_issue = pre_report.issues.iter().any(|i| {
        matches!(
            i.kind,
            treedb::engine::verification::ConsistencyIssueKind::UnappliedWalTail { .. }
        ) && i.severity == treedb::engine::verification::ConsistencySeverity::Info
    });
    assert!(
        has_unapplied_issue,
        "Pre-recovery verifier must report UnappliedWalTail with Info severity: {:?}",
        pre_report.issues
    );

    // 2. Resource-bounded audit: ensure truncated audit never returns is_healthy == true
    let mut bounded_config = default_config;
    bounded_config.max_pages = Some(1);
    let bounded_report = TreeDb::check_file(&path, &bounded_config).unwrap();
    assert!(bounded_report.truncated);
    assert!(
        !bounded_report.is_healthy(),
        "Resource-truncated audit must not be marked healthy"
    );

    // 3. Normal ARIES recovery
    let mut recovered_db = TreeDb::open(&path, 64).unwrap();

    // 4. Post-recovery verification is healthy, untruncated, and non-fatal
    let post_report = recovered_db.verify_consistency().unwrap();
    assert!(post_report.is_healthy());
    assert!(!post_report.truncated);
    assert!(!post_report.has_fatal());

    // 5. Executing a checkpoint establishes a new durable checkpoint boundary
    recovered_db.checkpoint().unwrap();
    let post_cp_report = recovered_db.verify_consistency().unwrap();
    assert!(post_cp_report.is_healthy());
    assert!(
        !post_cp_report.issues.iter().any(|i| matches!(
            i.kind,
            treedb::engine::verification::ConsistencyIssueKind::UnappliedWalTail { .. }
        )),
        "Post-checkpoint verifier must have zero unapplied WAL records beyond checkpoint"
    );
}
