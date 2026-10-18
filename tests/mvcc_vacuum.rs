use std::fs;
use std::path::PathBuf;

use treedb::engine::isolation::{CommitSeq, ReclamationHorizon, SnapshotRegistry};
use treedb::engine::types::{Row, Value};
use treedb::engine::vacuum::{
    evaluate_version_eligibility, VacuumConfig, VersionReclamationEligibility,
};
use treedb::storage::record_store::VersionMetadata;
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_mvcc_vac_{name}.treedb"));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        Self {
            _dir: dir,
            db_path,
            wal_path,
        }
    }

    fn db(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(wal_path_for(&self.db_path));
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

fn wal_path_for(db_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.wal", db_path.display()))
}

// =============================================================================
// 1. Reclamation Horizon Tests
// =============================================================================

#[test]
fn test_reclamation_horizon_empty_and_active_snapshots() {
    let mut reg = SnapshotRegistry::new();
    let cur_seq = CommitSeq(50);

    // With no active snapshots, horizon matches current_commit_seq
    let h0 = reg.reclamation_horizon(cur_seq);
    assert_eq!(h0.oldest_required_commit_seq, cur_seq);

    // Register S1 observing seq 10
    let s1 = reg.register(CommitSeq(10));
    assert_eq!(
        reg.reclamation_horizon(cur_seq).oldest_required_commit_seq,
        CommitSeq(10)
    );

    // Register S2 observing seq 25, S3 observing seq 40
    let s2 = reg.register(CommitSeq(25));
    let s3 = reg.register(CommitSeq(40));

    // Oldest active snapshot is still 10
    assert_eq!(
        reg.reclamation_horizon(cur_seq).oldest_required_commit_seq,
        CommitSeq(10)
    );

    // Release S1 -> horizon advances to 25
    reg.release(s1.snapshot_id);
    assert_eq!(
        reg.reclamation_horizon(cur_seq).oldest_required_commit_seq,
        CommitSeq(25)
    );

    // Release S2 -> horizon advances to 40
    reg.release(s2.snapshot_id);
    assert_eq!(
        reg.reclamation_horizon(cur_seq).oldest_required_commit_seq,
        CommitSeq(40)
    );

    // Release S3 -> horizon advances to current commit sequence 50
    reg.release(s3.snapshot_id);
    assert_eq!(
        reg.reclamation_horizon(cur_seq).oldest_required_commit_seq,
        cur_seq
    );
}

#[test]
fn test_version_eligibility_predicate_isolation() {
    let horizon = ReclamationHorizon::new(CommitSeq(50));
    let active_snapshots = vec![];
    let active_txns = std::collections::HashSet::new();

    // 1. Live head version without deletion -> LiveHead
    let live_meta = VersionMetadata::new_committed(TxnId(1), CommitSeq(30), None);
    let elig =
        evaluate_version_eligibility(true, &live_meta, horizon, &active_snapshots, &active_txns);
    assert_eq!(elig, VersionReclamationEligibility::LiveHead);
    assert!(!elig.is_eligible());

    // 2. Predecessor version deleted at 40 (<= horizon 50) -> EligibleSuperseded
    let mut pred_meta = VersionMetadata::new_committed(TxnId(1), CommitSeq(20), None);
    pred_meta.deleted_at = Some(CommitSeq(40));
    let elig =
        evaluate_version_eligibility(false, &pred_meta, horizon, &active_snapshots, &active_txns);
    assert_eq!(
        elig,
        VersionReclamationEligibility::EligibleSuperseded {
            deleted_at: CommitSeq(40)
        }
    );
    assert!(elig.is_eligible());

    // 3. Predecessor version deleted at 60 (> horizon 50) -> CreatedAfterHorizon
    let mut newer_pred_meta = VersionMetadata::new_committed(TxnId(1), CommitSeq(20), None);
    newer_pred_meta.deleted_at = Some(CommitSeq(60));
    let elig = evaluate_version_eligibility(
        false,
        &newer_pred_meta,
        horizon,
        &active_snapshots,
        &active_txns,
    );
    assert_eq!(
        elig,
        VersionReclamationEligibility::CreatedAfterHorizon {
            created_at: CommitSeq(60)
        }
    );
    assert!(!elig.is_eligible());

    // 4. Deleted logical row head deleted at 45 (<= horizon 50) -> EligibleDeletedRow
    let mut deleted_head = VersionMetadata::new_committed(TxnId(1), CommitSeq(30), None);
    deleted_head.deleted_at = Some(CommitSeq(45));
    let elig = evaluate_version_eligibility(
        true,
        &deleted_head,
        horizon,
        &active_snapshots,
        &active_txns,
    );
    assert_eq!(
        elig,
        VersionReclamationEligibility::EligibleDeletedRow {
            deleted_at: CommitSeq(45)
        }
    );
    assert!(elig.is_eligible());

    // 5. Aborted uncommitted version -> EligibleAborted
    let aborted_meta = VersionMetadata::new(TxnId(99), None);
    let elig = evaluate_version_eligibility(
        true,
        &aborted_meta,
        horizon,
        &active_snapshots,
        &active_txns,
    );
    assert_eq!(
        elig,
        VersionReclamationEligibility::EligibleAborted { xmin: TxnId(99) }
    );
    assert!(elig.is_eligible());
}

// =============================================================================
// 2. Live Version Protection & Historical Pruning
// =============================================================================

#[test]
fn test_vacuum_live_version_protection() {
    let ctx = TestContext::new("live_protection");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, val TEXT)")
        .unwrap();
    db.execute_statement("INSERT INTO t1 VALUES (1, 'initial')")
        .unwrap();

    let res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(res.versions_reclaimed, 0);

    // Verify row is live and readable
    let mut txn = db.begin_transaction().unwrap();
    let mut table = txn.open_table("t1").unwrap();
    let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String("initial".to_string()));
    txn.commit().unwrap();

    db.validate_vacuum_consistency().unwrap();
}

#[test]
fn test_vacuum_historical_version_pruning() {
    let ctx = TestContext::new("hist_pruning");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, val TEXT)")
        .unwrap();

    // Insert v1
    db.execute_statement("INSERT INTO t1 VALUES (1, 'v1')")
        .unwrap();
    // Update to v2
    db.execute_statement("UPDATE t1 SET val = 'v2' WHERE id = 1")
        .unwrap();
    // Update to v3
    db.execute_statement("UPDATE t1 SET val = 'v3' WHERE id = 1")
        .unwrap();

    // Verify before vacuum: version chain depth is 3
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("t1").unwrap();
        let depth = table
            .version_chain_depth(&Value::UInt64(1))
            .unwrap()
            .unwrap();
        assert_eq!(depth, 3);
        txn.commit().unwrap();
    }

    // Run vacuum with no active snapshots
    let res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(res.versions_reclaimed, 2);

    // Verify after vacuum: version chain depth is 1 (v1 and v2 pruned, v3 retained)
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("t1").unwrap();
        let depth = table
            .version_chain_depth(&Value::UInt64(1))
            .unwrap()
            .unwrap();
        assert_eq!(depth, 1);

        let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::String("v3".to_string()));
        txn.commit().unwrap();
    }

    // Repeated vacuum: 0 versions reclaimed (idempotent)
    let res2 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(res2.versions_reclaimed, 0);

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 3. Active Snapshot Isolation
// =============================================================================

#[test]
fn test_vacuum_active_snapshot_isolation_interleaved() {
    let ctx = TestContext::new("active_snap_iso");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, val TEXT)")
        .unwrap();
    db.execute_statement("INSERT INTO t1 VALUES (1, 'version_A')")
        .unwrap();

    // Register a standalone snapshot S1 observing version_A
    let s1 = db.register_snapshot();

    // Now update row to version_B in a committed transaction
    let mut t2 = db.begin_transaction().unwrap();
    t2.execute_statement("UPDATE t1 SET val = 'version_B' WHERE id = 1")
        .unwrap();
    t2.commit().unwrap();

    // Run vacuum: S1 is still active at version_A commit sequence!
    // Vacuum MUST NOT reclaim version_A!
    let vac_res1 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(vac_res1.versions_reclaimed, 0);
    assert!(vac_res1.blocked_by_oldest_snapshot);

    // Verify S1 can still read version_A!
    {
        let mut t_read = db.begin_transaction().unwrap();
        t_read.set_snapshot(s1);
        let mut table = t_read.open_table("t1").unwrap();
        let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::String("version_A".to_string()));
        t_read.commit().unwrap();
    }

    // Update again to version_C
    let mut t3 = db.begin_transaction().unwrap();
    t3.execute_statement("UPDATE t1 SET val = 'version_C' WHERE id = 1")
        .unwrap();
    t3.commit().unwrap();

    // Vacuum again while S1 is still active
    let _ = db.vacuum(&VacuumConfig::unlimited()).unwrap();

    // Verify S1 still reads version_A
    {
        let mut t_read = db.begin_transaction().unwrap();
        t_read.set_snapshot(s1);
        let mut table = t_read.open_table("t1").unwrap();
        let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::String("version_A".to_string()));
        t_read.commit().unwrap();
    }

    // Now S1 is released
    db.release_snapshot(s1.snapshot_id);

    // Run vacuum now that no snapshots require historical data
    let vac_res2 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert!(vac_res2.versions_reclaimed >= 1);

    // Current query sees version_C
    let mut t_latest = db.begin_transaction().unwrap();
    let mut table = t_latest.open_table("t1").unwrap();
    let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String("version_C".to_string()));
    t_latest.commit().unwrap();

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 4. Deleted Logical Records & Primary Key Reuse
// =============================================================================

#[test]
fn test_vacuum_deleted_logical_row_and_pk_reuse() {
    let ctx = TestContext::new("del_pk_reuse");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE users (id UINT64 PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_users_name ON users(name)")
        .unwrap();

    db.execute_statement("INSERT INTO users VALUES (10, 'Alice')")
        .unwrap();
    db.execute_statement("DELETE FROM users WHERE id = 10")
        .unwrap();

    // Before vacuum, primary index still points to the deleted record slot
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("users").unwrap();
        let exists = table
            .contains_primary_key_entry(&Value::UInt64(10))
            .unwrap();
        assert!(exists);
        txn.commit().unwrap();
    }

    // Run vacuum
    let vac_res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(vac_res.versions_reclaimed, 1);
    assert_eq!(vac_res.index_entries_removed, 1);

    // After vacuum, entry is completely gone from primary B+ tree
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("users").unwrap();
        let exists = table
            .contains_primary_key_entry(&Value::UInt64(10))
            .unwrap();
        assert!(!exists);
        txn.commit().unwrap();
    }

    // Primary key 10 can now be cleanly reinserted
    db.execute_statement("INSERT INTO users VALUES (10, 'Bob')")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut table = txn.open_table("users").unwrap();
    let row = table.get(&Value::UInt64(10)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String("Bob".to_string()));
    txn.commit().unwrap();

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 5. Secondary & Covering Indexes
// =============================================================================

#[test]
fn test_vacuum_secondary_and_covering_indexes_equivalence() {
    let ctx = TestContext::new("sec_idx_vac");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE accounts (id UINT64 PRIMARY KEY, balance INT64, branch TEXT)",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_balance ON accounts(balance)")
        .unwrap();

    // Insert 5 accounts
    for i in 1..=5 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, {}, 'branch_{i}')",
            i * 100
        ))
        .unwrap();
    }

    // Update account 1 multiple times
    db.execute_statement("UPDATE accounts SET balance = 150 WHERE id = 1")
        .unwrap();
    db.execute_statement("UPDATE accounts SET balance = 200 WHERE id = 1")
        .unwrap();

    // Delete account 2
    db.execute_statement("DELETE FROM accounts WHERE id = 2")
        .unwrap();

    // Record index query results before vacuum
    let rows_before: Vec<Row> = {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("accounts").unwrap();
        let r = table
            .index_lookup("idx_balance", &Value::Int64(200))
            .unwrap();
        txn.commit().unwrap();
        r
    };
    // id=1 has balance 200, id=2 was deleted so should only return id=1
    assert_eq!(rows_before.len(), 1);

    // Run vacuum
    let vac_res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert!(vac_res.versions_reclaimed >= 2);
    assert!(vac_res.index_entries_removed >= 2);

    // Verify index lookup after vacuum matches full table scan
    let mut txn = db.begin_transaction().unwrap();
    let mut table = txn.open_table("accounts").unwrap();
    let rows_after = table
        .index_lookup("idx_balance", &Value::Int64(200))
        .unwrap();
    let scan_rows = table.scan(None, None).unwrap();
    let matching_scan: Vec<Row> = scan_rows
        .into_iter()
        .filter(|r| r.values[1] == Value::Int64(200))
        .collect();

    assert_eq!(rows_after.len(), matching_scan.len());
    for (r_idx, r_scan) in rows_after.iter().zip(matching_scan.iter()) {
        assert_eq!(r_idx.values, r_scan.values);
    }
    txn.commit().unwrap();

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 6. Overflow Pages Reclamation
// =============================================================================

#[test]
fn test_vacuum_overflow_page_reclamation() {
    let ctx = TestContext::new("overflow_reclaim");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE docs (id UINT64 PRIMARY KEY, content TEXT)")
        .unwrap();

    // 10KB string requires multiple overflow pages (> 4070 bytes per page)
    let large_text = "X".repeat(10000);
    db.execute_statement(&format!("INSERT INTO docs VALUES (1, '{large_text}')"))
        .unwrap();

    let free_pages_before = db.free_page_count();

    // Update with another large string (creating new overflow chain, obsoleting first)
    let large_text_v2 = "Y".repeat(10000);
    db.execute_statement(&format!(
        "UPDATE docs SET content = '{large_text_v2}' WHERE id = 1"
    ))
    .unwrap();

    // Run vacuum
    let vac_res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(vac_res.versions_reclaimed, 1);
    assert!(vac_res.overflow_pages_reclaimed >= 2);

    let free_pages_after = db.free_page_count();
    assert!(
        free_pages_after >= free_pages_before + vac_res.overflow_pages_reclaimed as u32,
        "Freelist should account for reclaimed overflow pages"
    );

    // Verify current version still readable
    let mut txn = db.begin_transaction().unwrap();
    let mut table = txn.open_table("docs").unwrap();
    let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
    assert_eq!(row.values[1], Value::String(large_text_v2));
    txn.commit().unwrap();

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 7. Empty Record Page Reclamation
// =============================================================================

#[test]
fn test_vacuum_empty_record_page_reclamation() {
    let ctx = TestContext::new("empty_page_reclaim");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE items (id UINT64 PRIMARY KEY, payload TEXT)")
        .unwrap();

    // Insert 3 records of 1500 bytes each -> fits across multiple pages
    let p = "A".repeat(1500);
    db.execute_statement(&format!("INSERT INTO items VALUES (1, '{p}')"))
        .unwrap();
    db.execute_statement(&format!("INSERT INTO items VALUES (2, '{p}')"))
        .unwrap();
    db.execute_statement(&format!("INSERT INTO items VALUES (3, '{p}')"))
        .unwrap();

    // Delete all of them
    db.execute_statement("DELETE FROM items WHERE id = 1")
        .unwrap();
    db.execute_statement("DELETE FROM items WHERE id = 2")
        .unwrap();
    db.execute_statement("DELETE FROM items WHERE id = 3")
        .unwrap();

    // Run vacuum
    let vac_res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(vac_res.versions_reclaimed, 3);
    assert!(
        vac_res.pages_reclaimed >= 1,
        "At least one record page should be reclaimed into freelist"
    );

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 8. Bounded Resource Limits & Determinism
// =============================================================================

#[test]
fn test_vacuum_bounded_resource_limits() {
    let ctx = TestContext::new("bounded_limits");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE counters (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();
    db.execute_statement("INSERT INTO counters VALUES (1, 0)")
        .unwrap();

    // 10 consecutive updates
    for i in 1..=10 {
        db.execute_statement(&format!("UPDATE counters SET val = {i} WHERE id = 1"))
            .unwrap();
    }

    // Run vacuum with max_versions = 3
    let cfg = VacuumConfig::bounded(3, 10);
    let res1 = db.vacuum(&cfg).unwrap();
    assert!(res1.versions_examined <= 3);

    // Another bounded pass
    let res2 = db.vacuum(&cfg).unwrap();
    assert!(res2.versions_examined <= 3);

    // Full unlimited pass to finish reclamation
    let res3 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert!(res3.versions_examined > 0);

    // Subsequent unlimited pass yields 0 reclaimed
    let res4 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
    assert_eq!(res4.versions_reclaimed, 0);

    db.validate_vacuum_consistency().unwrap();
}

// =============================================================================
// 9. Idempotency & Crash Recovery
// =============================================================================

#[test]
fn test_vacuum_idempotency_across_reopen() {
    let ctx = TestContext::new("idempotency");

    {
        let mut db = ctx.db();
        db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, val TEXT)")
            .unwrap();
        db.execute_statement("INSERT INTO t1 VALUES (1, 'initial')")
            .unwrap();
        db.execute_statement("UPDATE t1 SET val = 'updated' WHERE id = 1")
            .unwrap();

        let res1 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
        assert_eq!(res1.versions_reclaimed, 1);

        let res2 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
        assert_eq!(res2.versions_reclaimed, 0);

        let res3 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
        assert_eq!(res3.versions_reclaimed, 0);
    }

    // Reopen database
    {
        let mut db = ctx.db();
        db.validate_vacuum_consistency().unwrap();

        let res4 = db.vacuum(&VacuumConfig::unlimited()).unwrap();
        assert_eq!(res4.versions_reclaimed, 0);

        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("t1").unwrap();
        let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::String("updated".to_string()));
        txn.commit().unwrap();
    }
}

#[test]
fn test_vacuum_crash_recovery_durability() {
    let ctx = TestContext::new("crash_recovery");

    {
        let mut db = ctx.db();
        db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, val TEXT)")
            .unwrap();
        db.execute_statement("INSERT INTO t1 VALUES (1, 'v1')")
            .unwrap();
        db.execute_statement("UPDATE t1 SET val = 'v2' WHERE id = 1")
            .unwrap();

        // Vacuum runs within transaction and commits
        let res = db.vacuum(&VacuumConfig::unlimited()).unwrap();
        assert_eq!(res.versions_reclaimed, 1);
        // Explicitly checkpoint and truncate WAL to test persistence
        db.checkpoint_and_truncate_wal().unwrap();
    }

    // Reopen from disk without clean close
    {
        let mut db = ctx.db();
        db.validate_vacuum_consistency().unwrap();

        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("t1").unwrap();
        let row = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::String("v2".to_string()));
        txn.commit().unwrap();
    }
}
