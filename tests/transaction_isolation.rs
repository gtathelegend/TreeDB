//! Phase 67 Integration Tests: Transaction Isolation Contract, Snapshot Architecture & MVCC Preparation.
//!
//! Validates:
//! 1. IsolationLevel abstraction (defaults, configuration, status classification).
//! 2. Snapshot abstraction (immutability, deterministic allocation, registry, oldest active snapshot).
//! 3. Logical CommitSeq allocation (monotonicity on commit, no consumption on abort, separation from LSN).
//! 4. Transaction state machine (transitions, invalid transitions rejection, Drop auto-rollback).
//! 5. Visibility policy (CurrentVersionVisibility, own writes, committed writes, rollback invisibility).
//! 6. Transaction write set abstraction (insert, update, delete, table mutations).
//! 7. Execution context integration & prepared statement isolation (reusable physical plan, no snapshot leakage).
//! 8. Plan cache & adaptive feedback fingerprint independence from snapshot identity.
//! 9. Deterministic interleaving and restart monotonicity without concurrency.

use tempfile::tempdir;
use treedb::engine::isolation::{
    CommitSeq, CurrentVersionVisibility, IsolationContext, IsolationLevel, IsolationStatus,
    ReadSet, Snapshot, SnapshotId, SnapshotRegistry, TransactionCommitMetadata,
    TransactionWriteSet, VisibilityPolicy,
};
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Error;
use treedb::index::RecordId;
use treedb::query::execution::ExecutionContext;
use treedb::query::fingerprint::PhysicalPlanFingerprint;
use treedb::wal::lsn::TxnId;
use treedb::{TreeDb, TxnState};

fn create_test_db() -> (TreeDb, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_isolation.db");
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir)
}

// ============================================================================
// 1. Isolation Level Abstraction Tests
// ============================================================================

#[test]
fn test_isolation_level_defaults_and_naming() {
    let default_level = IsolationLevel::default();
    assert_eq!(default_level, IsolationLevel::ReadCommitted);
    assert_eq!(default_level.name(), "READ COMMITTED");
    assert_eq!(format!("{default_level}"), "READ COMMITTED");
    assert_eq!(default_level.status(), IsolationStatus::PartiallyPrepared);

    let rr = IsolationLevel::RepeatableRead;
    assert_eq!(rr.name(), "REPEATABLE READ");
    assert_eq!(format!("{rr}"), "REPEATABLE READ");
    assert_eq!(rr.status(), IsolationStatus::PartiallyPrepared);

    let serializable = IsolationLevel::Serializable;
    assert_eq!(serializable.name(), "SERIALIZABLE");
    assert_eq!(format!("{serializable}"), "SERIALIZABLE");
    assert_eq!(serializable.status(), IsolationStatus::DefinedNotEnforced);
}

#[test]
fn test_transaction_begins_with_configured_isolation_level() {
    let (mut db, _dir) = create_test_db();

    // Default begin uses ReadCommitted
    let txn1 = db.begin().unwrap();
    assert_eq!(
        txn1.isolation_context().isolation_level,
        IsolationLevel::ReadCommitted
    );
    txn1.commit().unwrap();

    // Explicit begin with RepeatableRead
    let txn2 = db
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    assert_eq!(
        txn2.isolation_context().isolation_level,
        IsolationLevel::RepeatableRead
    );
    txn2.commit().unwrap();

    // Explicit begin with Serializable
    let txn3 = db
        .begin_transaction_with_isolation(IsolationLevel::Serializable)
        .unwrap();
    assert_eq!(
        txn3.isolation_context().isolation_level,
        IsolationLevel::Serializable
    );
    txn3.commit().unwrap();
}

// ============================================================================
// 2. Snapshot Abstraction Tests
// ============================================================================

#[test]
fn test_snapshot_immutability_and_identity() {
    let (mut db, _dir) = create_test_db();

    let txn = db.begin().unwrap();
    let snap1 = *txn.snapshot();
    assert_eq!(snap1.snapshot_id, SnapshotId(1));
    assert_eq!(snap1.as_of_commit_seq, CommitSeq(1));

    // Snapshot is Copy and immutable
    let snap1_copy = snap1;
    assert_eq!(snap1, snap1_copy);
    assert_eq!(format!("{snap1}"), "Snapshot(id=1, as_of=1)");

    txn.commit().unwrap();

    // Next transaction gets a distinct snapshot ID and advanced commit seq horizon
    let txn2 = db.begin().unwrap();
    let snap2 = *txn2.snapshot();
    assert_eq!(snap2.snapshot_id, SnapshotId(2));
    assert_eq!(snap2.as_of_commit_seq, CommitSeq(2));
    assert_ne!(snap1.snapshot_id, snap2.snapshot_id);
    txn2.commit().unwrap();
}

#[test]
fn test_snapshot_registry_and_oldest_active() {
    let mut registry = SnapshotRegistry::new();
    assert_eq!(registry.active_count(), 0);
    assert_eq!(registry.oldest_active_snapshot(), None);

    let s1 = registry.register(CommitSeq(1));
    assert_eq!(s1.snapshot_id, SnapshotId(1));
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.oldest_active_snapshot(), Some(SnapshotId(1)));
    assert!(registry.is_active(SnapshotId(1)));

    let s2 = registry.register(CommitSeq(2));
    assert_eq!(s2.snapshot_id, SnapshotId(2));
    assert_eq!(registry.active_count(), 2);
    // Oldest active snapshot remains SnapshotId(1)
    assert_eq!(registry.oldest_active_snapshot(), Some(SnapshotId(1)));

    let s3 = registry.register(CommitSeq(3));
    assert_eq!(s3.snapshot_id, SnapshotId(3));
    assert_eq!(registry.active_count(), 3);
    assert_eq!(registry.oldest_active_snapshot(), Some(SnapshotId(1)));

    // Releasing oldest snapshot advances oldest_active to s2
    registry.release(s1.snapshot_id);
    assert_eq!(registry.active_count(), 2);
    assert_eq!(registry.oldest_active_snapshot(), Some(SnapshotId(2)));
    assert!(!registry.is_active(SnapshotId(1)));
    assert!(registry.is_active(SnapshotId(2)));

    // Releasing s2 advances oldest_active to s3
    registry.release(s2.snapshot_id);
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.oldest_active_snapshot(), Some(SnapshotId(3)));

    // Releasing s3 leaves registry empty
    registry.release(s3.snapshot_id);
    assert_eq!(registry.active_count(), 0);
    assert_eq!(registry.oldest_active_snapshot(), None);
}

#[test]
fn test_statement_snapshot_refresh() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin().unwrap();

    let initial_snap = *txn.snapshot();
    assert_eq!(initial_snap.snapshot_id, SnapshotId(1));

    // Refresh statement snapshot
    let refreshed_snap = txn.statement_snapshot();
    assert_eq!(refreshed_snap.snapshot_id, SnapshotId(2));
    assert_eq!(txn.snapshot().snapshot_id, SnapshotId(2));

    txn.commit().unwrap();
}

// ============================================================================
// 3. Commit Sequence Monotonicity & Ordering Tests
// ============================================================================

#[test]
fn test_commit_seq_monotonic_allocation() {
    let (mut db, _dir) = create_test_db();

    assert_eq!(db.current_commit_seq(), CommitSeq(1));

    // Commit advances commit sequence
    let mut txn1 = db.begin().unwrap();
    txn1.put(1, b"val1").unwrap();
    txn1.commit().unwrap();
    assert_eq!(db.current_commit_seq(), CommitSeq(2));

    // Another commit advances again
    let mut txn2 = db.begin().unwrap();
    txn2.put(2, b"val2").unwrap();
    txn2.commit().unwrap();
    assert_eq!(db.current_commit_seq(), CommitSeq(3));

    // Abort does NOT consume or advance commit sequence
    let mut txn3 = db.begin().unwrap();
    txn3.put(3, b"val3").unwrap();
    txn3.abort().unwrap();
    assert_eq!(db.current_commit_seq(), CommitSeq(3));

    // Subsequent commit advances from unchanged commit sequence
    let mut txn4 = db.begin().unwrap();
    txn4.put(4, b"val4").unwrap();
    txn4.commit().unwrap();
    assert_eq!(db.current_commit_seq(), CommitSeq(4));
}

#[test]
fn test_commit_metadata_lifecycle_representation() {
    let meta_active = TransactionCommitMetadata::active(TxnId(1));
    assert_eq!(meta_active.state, TxnState::Active);
    assert_eq!(meta_active.commit_seq, None);

    let meta_committed = TransactionCommitMetadata::committed(TxnId(1), CommitSeq(42));
    assert_eq!(meta_committed.state, TxnState::Committed);
    assert_eq!(meta_committed.commit_seq, Some(CommitSeq(42)));

    let meta_aborted = TransactionCommitMetadata::aborted(TxnId(1));
    assert_eq!(meta_aborted.state, TxnState::Aborted);
    assert_eq!(meta_aborted.commit_seq, None);
}

// ============================================================================
// 4. Transaction State Machine & Lifecycle Tests
// ============================================================================

#[test]
fn test_txn_state_transitions() {
    assert_eq!(
        TxnState::Created.transition_to(TxnState::Active).unwrap(),
        TxnState::Active
    );
    assert_eq!(
        TxnState::Created.transition_to(TxnState::Aborted).unwrap(),
        TxnState::Aborted
    );
    assert_eq!(
        TxnState::Active.transition_to(TxnState::Committed).unwrap(),
        TxnState::Committed
    );
    assert_eq!(
        TxnState::Active.transition_to(TxnState::Aborted).unwrap(),
        TxnState::Aborted
    );

    // Invalid transitions
    assert!(TxnState::Committed.transition_to(TxnState::Active).is_err());
    assert!(TxnState::Committed
        .transition_to(TxnState::Committed)
        .is_err());
    assert!(TxnState::Committed
        .transition_to(TxnState::Aborted)
        .is_err());
    assert!(TxnState::Aborted.transition_to(TxnState::Active).is_err());
    assert!(TxnState::Aborted
        .transition_to(TxnState::Committed)
        .is_err());
    assert!(TxnState::Aborted.transition_to(TxnState::Aborted).is_err());
}

#[test]
fn test_transaction_lifecycle_rejection_of_invalid_transitions() {
    let (mut db, _dir) = create_test_db();

    // 1. Committed -> commit & Committed -> abort
    {
        let mut txn1 = db.begin().unwrap();
        txn1.commit_in_place().unwrap();
        assert_eq!(txn1.state(), TxnState::Committed);

        let err1 = txn1.commit_in_place().unwrap_err();
        assert!(
            matches!(err1, Error::InvalidTransactionState { ref current, ref action } if current == "Committed" && action == "commit")
        );

        let err2 = txn1.abort_in_place().unwrap_err();
        assert!(
            matches!(err2, Error::InvalidTransactionState { ref current, ref action } if current == "Committed" && action == "abort")
        );

        // Also get/put on committed transaction fails with TransactionAlreadyCommitted
        let err3 = txn1.get(1).unwrap_err();
        assert!(matches!(err3, Error::TransactionAlreadyCommitted(_)));
    }

    // 2. Aborted -> commit & Aborted -> abort
    {
        let mut txn2 = db.begin().unwrap();
        txn2.abort_in_place().unwrap();
        assert_eq!(txn2.state(), TxnState::Aborted);

        let err4 = txn2.commit_in_place().unwrap_err();
        assert!(
            matches!(err4, Error::InvalidTransactionState { ref current, ref action } if current == "Aborted" && action == "commit")
        );

        let err5 = txn2.abort_in_place().unwrap_err();
        assert!(
            matches!(err5, Error::InvalidTransactionState { ref current, ref action } if current == "Aborted" && action == "abort")
        );

        // Also get/put on aborted transaction fails with TransactionAlreadyAborted
        let err6 = txn2.put(1, b"val").unwrap_err();
        assert!(matches!(err6, Error::TransactionAlreadyAborted(_)));
    }
}

#[test]
fn test_transaction_drop_auto_abort_and_registry_cleanup() {
    let (mut db, _dir) = create_test_db();

    assert_eq!(db.active_snapshot_count(), 0);

    {
        let mut txn = db.begin().unwrap();
        assert_eq!(txn.db().active_snapshot_count(), 1);
        txn.put(100, b"uncommitted_drop").unwrap();
        // Dropping txn here without commit
    }

    // Registry was cleaned up on drop
    assert_eq!(db.active_snapshot_count(), 0);
    assert_eq!(db.oldest_active_snapshot(), None);

    // Physical modifications were cleanly rolled back
    let mut verify_txn = db.begin().unwrap();
    assert_eq!(verify_txn.get(100).unwrap(), None);
    verify_txn.commit().unwrap();
}

// ============================================================================
// 5. Visibility Policy Tests
// ============================================================================

#[test]
fn test_current_version_visibility_policy() {
    let policy = CurrentVersionVisibility;
    let reader_ctx = IsolationContext::new(
        IsolationLevel::ReadCommitted,
        TxnId(10),
        Snapshot::new(SnapshotId(1), CommitSeq(5)),
    );

    // 1. Own uncommitted write is visible
    assert!(policy.is_visible_to_context(TxnId(10), None, &reader_ctx));

    // 2. Own committed write is visible
    assert!(policy.is_visible_to_context(TxnId(10), Some(CommitSeq(3)), &reader_ctx));
    assert!(policy.is_visible_to_context(TxnId(10), Some(CommitSeq(8)), &reader_ctx));

    // 3. Other transaction uncommitted write is invisible
    assert!(!policy.is_visible_to_context(TxnId(20), None, &reader_ctx));

    // 4. Other transaction committed prior to snapshot horizon is visible
    assert!(policy.is_visible_to_context(TxnId(20), Some(CommitSeq(4)), &reader_ctx));
    assert!(policy.is_visible_to_context(TxnId(20), Some(CommitSeq(5)), &reader_ctx));

    // 5. Other transaction committed after snapshot horizon is invisible
    assert!(!policy.is_visible_to_context(TxnId(20), Some(CommitSeq(6)), &reader_ctx));
}

#[test]
fn test_visibility_policy_trait_evaluation() {
    let policy = CurrentVersionVisibility;
    let snap = Snapshot::new(SnapshotId(1), CommitSeq(10));

    // Null transaction (engine-created initial pages) is visible
    assert!(policy.is_visible(treedb::wal::lsn::NULL_TXN, None, &snap));

    // Committed prior to horizon is visible
    assert!(policy.is_visible(TxnId(5), Some(CommitSeq(8)), &snap));

    // Committed after horizon is invisible
    assert!(!policy.is_visible(TxnId(5), Some(CommitSeq(12)), &snap));

    // Uncommitted is invisible through raw trait method
    assert!(!policy.is_visible(TxnId(5), None, &snap));
}

// ============================================================================
// 6. Transaction Write Set Abstraction Tests
// ============================================================================

#[test]
fn test_transaction_write_set_tracking() {
    let mut ws = TransactionWriteSet::new();
    assert!(ws.is_empty());
    assert_eq!(ws.total_mutations(), 0);

    let rid1 = RecordId::new(5, 0);
    let rid2 = RecordId::new(5, 1);
    let rid3 = RecordId::new(6, 0);

    ws.record_insert(rid1);
    assert!(ws.contains(rid1));
    assert!(ws.is_inserted(rid1));
    assert!(!ws.is_updated(rid1));
    assert!(!ws.is_deleted(rid1));
    assert_eq!(ws.inserted_count(), 1);

    ws.record_update(rid1, rid2);
    assert!(ws.contains(rid1));
    assert!(ws.contains(rid2));
    assert!(ws.is_updated(rid1));
    assert!(ws.is_inserted(rid2));
    assert_eq!(ws.updated_count(), 1);

    ws.record_delete(rid3);
    assert!(ws.contains(rid3));
    assert!(ws.is_deleted(rid3));
    assert_eq!(ws.deleted_count(), 1);

    ws.record_table_mutation("customers");
    assert!(!ws.is_empty());
    assert_eq!(ws.total_mutations(), 4);

    ws.clear();
    assert!(ws.is_empty());
    assert_eq!(ws.total_mutations(), 0);
}

#[test]
fn test_transaction_populates_write_set_on_mutations() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin().unwrap();

    // Initial write set is empty
    assert!(txn.write_set().is_empty());

    // Put inserts a record
    txn.put(1, b"hello").unwrap();
    assert_eq!(txn.write_set().inserted_count(), 1);

    // Put updates existing record
    txn.put(1, b"world").unwrap();
    assert_eq!(txn.write_set().updated_count(), 1);

    // Delete deletes record
    txn.delete(1).unwrap();
    assert_eq!(txn.write_set().deleted_count(), 1);

    // Create table records table mutation
    let schema = Schema::new(vec![Column::new("id", DataType::Int64, false)], 0).unwrap();
    txn.create_table("items", schema).unwrap();

    assert!(!txn.write_set().is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_read_set_abstraction() {
    let mut rs = ReadSet::new();
    assert!(rs.is_empty());

    rs.record_read(RecordId::new(10, 0));
    rs.record_page_read(20);
    rs.record_table_read("users");

    assert!(!rs.is_empty());
    assert_eq!(rs.records.len(), 1);
    assert_eq!(rs.pages.len(), 1);
    assert_eq!(rs.tables.len(), 1);

    rs.clear();
    assert!(rs.is_empty());
}

// ============================================================================
// 7. Execution Context & Prepared Statement Isolation Tests
// ============================================================================

#[test]
fn test_execution_context_carries_isolation_context_and_snapshot() {
    let iso_ctx = IsolationContext::new(
        IsolationLevel::RepeatableRead,
        TxnId(7),
        Snapshot::new(SnapshotId(42), CommitSeq(99)),
    );

    let exec_ctx = ExecutionContext::for_txn_with_isolation(TxnId(7), iso_ctx.clone());

    assert_eq!(exec_ctx.isolation_context(), &iso_ctx);
    assert_eq!(
        exec_ctx.snapshot(),
        &Snapshot::new(SnapshotId(42), CommitSeq(99))
    );
    assert_eq!(exec_ctx.isolation_level(), IsolationLevel::RepeatableRead);
}

#[test]
fn test_prepared_statement_reused_across_distinct_transactions() {
    let (mut db, _dir) = create_test_db();

    // 1. Setup table and initial data
    {
        let mut setup_txn = db.begin().unwrap();
        setup_txn
            .execute_statement("CREATE TABLE users (id INT PRIMARY KEY, age INT);")
            .unwrap();
        setup_txn
            .execute_statement("INSERT INTO users VALUES (1, 25), (2, 30), (3, 35);")
            .unwrap();
        setup_txn.commit().unwrap();
    }

    // 2. Prepare statement once
    let prepared = {
        let mut prep_txn = db.begin().unwrap();
        let p = prep_txn
            .prepare_sql("SELECT id, age FROM users WHERE age > ?;")
            .unwrap();
        prep_txn.commit().unwrap();
        p
    };

    // 3. Execute prepared statement in Transaction A (Snapshot A)
    let mut txn_a = db
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .unwrap();
    let snap_a = *txn_a.snapshot();
    let res_a = txn_a
        .execute_prepared_query(&prepared, &[Value::Int64(26)])
        .unwrap();
    txn_a.commit().unwrap();

    // 4. Execute prepared statement in Transaction B (Snapshot B)
    let mut txn_b = db
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    let snap_b = *txn_b.snapshot();
    let res_b = txn_b
        .execute_prepared_query(&prepared, &[Value::Int64(26)])
        .unwrap();
    txn_b.commit().unwrap();

    // Snapshots were distinct across transactions
    assert_ne!(snap_a.snapshot_id, snap_b.snapshot_id);

    // Results are identical
    assert_eq!(res_a.rows.len(), 2);
    assert_eq!(res_b.rows.len(), 2);
}

#[test]
fn test_plan_cache_and_fingerprints_have_no_snapshot_leakage() {
    let (mut db, _dir) = create_test_db();

    {
        let mut setup_txn = db.begin().unwrap();
        setup_txn
            .execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, amount INT);")
            .unwrap();
        setup_txn
            .execute_statement("INSERT INTO orders VALUES (1, 100), (2, 200);")
            .unwrap();
        setup_txn.commit().unwrap();
    }

    let sql = "SELECT id, amount FROM orders WHERE amount >= 100;";

    // Plan in Txn 1
    let mut txn1 = db.begin().unwrap();
    let plan1 = txn1.plan_sql(sql).unwrap();
    let fp1 = PhysicalPlanFingerprint::from_plan(&plan1);
    txn1.commit().unwrap();

    // Plan in Txn 2 with different isolation level
    let mut txn2 = db
        .begin_with_isolation(IsolationLevel::RepeatableRead)
        .unwrap();
    let plan2 = txn2.plan_sql(sql).unwrap();
    let fp2 = PhysicalPlanFingerprint::from_plan(&plan2);
    txn2.commit().unwrap();

    // Canonical representation and digest contain no SnapshotId or TxnId
    assert_eq!(fp1.digest(), fp2.digest());
    assert_eq!(fp1.canonical(), fp2.canonical());
    assert!(!fp1.canonical().contains("Snapshot"));
    assert!(!fp1.canonical().contains("TxnId"));
    assert!(!fp1.canonical().contains("CommitSeq"));
}

// ============================================================================
// 8. Deterministic Interleaving & Monotonicity Recovery Tests
// ============================================================================

#[test]
fn test_deterministic_interleaving_single_version_lifecycle() {
    let (mut db, _dir) = create_test_db();

    // Step 1: Txn 1 writes key/row and commits
    {
        let mut t1 = db.begin().unwrap();
        t1.put(10, b"first_value").unwrap();
        assert_eq!(t1.get(10).unwrap(), Some(b"first_value".to_vec()));
        t1.commit().unwrap();
    }

    // Step 2: Txn 2 begins, reads T1's commit, writes, then aborts
    {
        let mut t2 = db.begin().unwrap();
        assert_eq!(t2.get(10).unwrap(), Some(b"first_value".to_vec()));
        t2.put(10, b"aborted_overwrite").unwrap();
        assert_eq!(t2.get(10).unwrap(), Some(b"aborted_overwrite".to_vec()));
        t2.abort().unwrap();
    }

    // Step 3: Txn 3 begins and verifies rolled back overwrite restored first_value
    {
        let mut t3 = db.begin().unwrap();
        assert_eq!(t3.get(10).unwrap(), Some(b"first_value".to_vec()));
        t3.commit().unwrap();
    }
}

#[test]
fn test_recovery_monotonicity_and_clean_snapshot_registry() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_reopen.db");

    // Phase 1: create db, perform multiple commits and aborts
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        let mut t1 = db.begin().unwrap();
        t1.put(1, b"committed_1").unwrap();
        t1.commit().unwrap();
        assert_eq!(db.current_commit_seq(), CommitSeq(2));

        let mut t2 = db.begin().unwrap();
        t2.put(2, b"aborted_2").unwrap();
        t2.abort().unwrap();
        // Unchanged
        assert_eq!(db.current_commit_seq(), CommitSeq(2));

        let mut t3 = db.begin().unwrap();
        t3.put(3, b"committed_3").unwrap();
        t3.commit().unwrap();
        assert_eq!(db.current_commit_seq(), CommitSeq(3));

        // Registry is clean prior to close
        assert_eq!(db.active_snapshot_count(), 0);
    }

    // Phase 2: reopen database
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();

        // Registry starts empty
        assert_eq!(db.active_snapshot_count(), 0);
        assert_eq!(db.oldest_active_snapshot(), None);

        // Next transaction begins cleanly
        let mut t4 = db.begin().unwrap();
        assert_eq!(t4.get(1).unwrap(), Some(b"committed_1".to_vec()));
        assert_eq!(t4.get(2).unwrap(), None); // Aborted was undone
        assert_eq!(t4.get(3).unwrap(), Some(b"committed_3".to_vec()));
        t4.commit().unwrap();

        assert_eq!(db.active_snapshot_count(), 0);
    }
}
