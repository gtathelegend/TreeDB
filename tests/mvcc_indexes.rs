//! Phase 68 Integration Tests: Secondary Index MVCC Record Visibility & Constraints.
//!
//! Validates:
//! 1. Secondary index lookups pointing to the correct visible version after updates.
//! 2. Unique secondary index constraints allowing reinsertion after committed deletion.
//! 3. Unique secondary index constraints rolling back on transaction abort.
//! 4. Secondary index lookups filtering out versions invisible to historical snapshots.
//! 5. Multi-column composite secondary indexes pointing to correct visible versions.

use tempfile::tempdir;
use treedb::engine::isolation::{
    CommitSeq, IsolationContext, IsolationLevel, Snapshot, SnapshotId,
};
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

fn make_user_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("email", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )
    .unwrap()
}

fn create_test_db(name: &str) -> (TreeDb, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.db"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir)
}

#[test]
fn test_secondary_index_lookup_points_to_visible_version() {
    let (mut db, _dir) = create_test_db("sec_idx_mvcc");

    db.create_table("users", make_user_schema()).unwrap();
    db.create_index("users", "idx_users_email", "email", true)
        .unwrap();

    // Insert row: id=1, email=alice@old.com
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("users").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("alice@old.com".into()),
            Value::String("Alice".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Update row: id=1, email=alice@new.com
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("users").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("alice@new.com".into()),
            Value::String("Alice Updated".into()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    // Verify through table methods
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("users").unwrap();
        let row_by_pk = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row_by_pk.values[1], Value::String("alice@new.com".into()));
        assert_eq!(row_by_pk.values[2], Value::String("Alice Updated".into()));
    }
    t3.commit().unwrap();

    // Verify via SQL query selecting on indexed column
    let res = db
        .execute_sql("SELECT name FROM users WHERE email = 'alice@new.com'")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice Updated".into()));

    let res_old = db
        .execute_sql("SELECT name FROM users WHERE email = 'alice@old.com'")
        .unwrap();
    assert_eq!(res_old.rows.len(), 0);
}

#[test]
fn test_unique_secondary_index_reinsertion_after_delete() {
    let (mut db, _dir) = create_test_db("unique_idx_reinsert");

    db.create_table("users", make_user_schema()).unwrap();
    db.create_index("users", "idx_users_email", "email", true)
        .unwrap();

    // Insert id=1, email=unique@test.com
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("users").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("unique@test.com".into()),
            Value::String("First".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Delete id=1
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("users").unwrap();
        let deleted = tbl.delete(&Value::Int64(1)).unwrap();
        assert!(deleted.is_some());
    }
    t2.commit().unwrap();

    // Insert new row id=2 with same email=unique@test.com -> must succeed because old version is deleted
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("users").unwrap();
        let insert_res = tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("unique@test.com".into()),
            Value::String("Second".into()),
        ]));
        assert!(
            insert_res.is_ok(),
            "re-insertion with previously deleted unique key must succeed"
        );
    }
    t3.commit().unwrap();

    // Verify lookup finds the second row
    let res = db
        .execute_sql("SELECT id, name FROM users WHERE email = 'unique@test.com'")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    assert_eq!(res.rows[0].values[1], Value::String("Second".into()));
}

#[test]
fn test_unique_index_uncommitted_update_rollback() {
    let (mut db, _dir) = create_test_db("unique_idx_rollback");

    db.create_table("users", make_user_schema()).unwrap();
    db.create_index("users", "idx_users_email", "email", true)
        .unwrap();

    // Insert id=1 with email=original@test.com
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("users").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("original@test.com".into()),
            Value::String("User 1".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Txn A updates id=1 to email=taken@test.com, then aborts
    let mut txn_a = db.begin().unwrap();
    {
        let mut tbl = txn_a.open_table("users").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("taken@test.com".into()),
            Value::String("User 1 Modified".into()),
        ]))
        .unwrap();
    }
    txn_a.abort().unwrap();

    // Txn B inserts new row with email=taken@test.com -> must succeed since Txn A aborted
    let mut txn_b = db.begin().unwrap();
    {
        let mut tbl = txn_b.open_table("users").unwrap();
        let res = tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("taken@test.com".into()),
            Value::String("User 2".into()),
        ]));
        assert!(
            res.is_ok(),
            "aborted update must release unique constraint on new key"
        );
    }
    txn_b.commit().unwrap();

    // Verify both original and new rows are correctly queryable
    let res1 = db
        .execute_sql("SELECT name FROM users WHERE email = 'original@test.com'")
        .unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[0], Value::String("User 1".into()));

    let res2 = db
        .execute_sql("SELECT name FROM users WHERE email = 'taken@test.com'")
        .unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("User 2".into()));
}

#[test]
fn test_secondary_index_historical_snapshot_filtering() {
    let (mut db, _dir) = create_test_db("sec_idx_history");

    db.create_table("users", make_user_schema()).unwrap();
    db.create_index("users", "idx_users_email", "email", true)
        .unwrap();

    // Seed row 1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("users").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("user@domain.com".into()),
            Value::String("Version 1".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Capture historical snapshot horizon
    let historical_horizon = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Update row 1 to Version 2
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("users").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("user@domain.com".into()),
            Value::String("Version 2".into()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    // Reading with historical horizon observes Version 1
    {
        let mut tbl = db.open_table("users").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(999),
            Snapshot::new(SnapshotId(1), historical_horizon),
        )));
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::String("Version 1".into()));

        // Also verify via index_lookup on the historical snapshot!
        let rows_via_idx = tbl
            .index_lookup("idx_users_email", &Value::String("user@domain.com".into()))
            .unwrap();
        assert_eq!(rows_via_idx.len(), 1);
        assert_eq!(rows_via_idx[0].values[2], Value::String("Version 1".into()));
    }

    // Reading with latest state observes Version 2
    {
        let mut tbl = db.open_table("users").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::String("Version 2".into()));

        let rows_via_idx = tbl
            .index_lookup("idx_users_email", &Value::String("user@domain.com".into()))
            .unwrap();
        assert_eq!(rows_via_idx.len(), 1);
        assert_eq!(rows_via_idx[0].values[2], Value::String("Version 2".into()));
    }
}

#[test]
fn test_secondary_index_lookup_when_indexed_key_changes() {
    let (mut db, _dir) = create_test_db("sec_idx_key_change");

    db.create_table("users", make_user_schema()).unwrap();
    db.create_index("users", "idx_users_email", "email", true)
        .unwrap();

    // V1: email = "old@example.com"
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("users").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("old@example.com".into()),
            Value::String("Version 1".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    let h1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // V2: update email to "new@example.com"
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("users").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("new@example.com".into()),
            Value::String("Version 2".into()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    let h2 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Snapshot H1:
    // WHERE email = 'old@example.com' -> returns V1
    // WHERE email = 'new@example.com' -> returns 0 rows
    {
        let mut tbl = db.open_table("users").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(101),
            Snapshot::new(SnapshotId(1), h1),
        )));
        let old_res = tbl
            .index_lookup("idx_users_email", &Value::String("old@example.com".into()))
            .unwrap();
        assert_eq!(old_res.len(), 1, "H1 snapshot must find V1 under old email");
        assert_eq!(old_res[0].values[2], Value::String("Version 1".into()));

        let new_res = tbl
            .index_lookup("idx_users_email", &Value::String("new@example.com".into()))
            .unwrap();
        assert_eq!(new_res.len(), 0, "H1 snapshot must not see new email");
    }

    // Snapshot H2:
    // WHERE email = 'old@example.com' -> returns 0 rows (V1 deleted)
    // WHERE email = 'new@example.com' -> returns V2
    {
        let mut tbl = db.open_table("users").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(102),
            Snapshot::new(SnapshotId(2), h2),
        )));
        let old_res = tbl
            .index_lookup("idx_users_email", &Value::String("old@example.com".into()))
            .unwrap();
        assert_eq!(
            old_res.len(),
            0,
            "H2 snapshot must not see old email for deleted version"
        );

        let new_res = tbl
            .index_lookup("idx_users_email", &Value::String("new@example.com".into()))
            .unwrap();
        assert_eq!(new_res.len(), 1, "H2 snapshot must find V2 under new email");
        assert_eq!(new_res[0].values[2], Value::String("Version 2".into()));
    }
}

#[test]
fn test_covering_index_consistency_no_payload_leakage() {
    let (mut db, _dir) = create_test_db("cov_idx_mvcc");

    // Table: id INT64 (PK), dept STRING, secret STRING
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, false),
            Column::new("secret", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    db.create_table("employees", schema).unwrap();

    // Covering index on dept INCLUDE (secret)
    db.execute_statement("CREATE INDEX idx_emp_cov ON employees (dept) INCLUDE (secret);")
        .unwrap();

    // V1: id=1, dept="Eng", secret="alpha"
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("employees").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("Eng".into()),
            Value::String("alpha".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    let snap1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // V2: update id=1, dept="Eng", secret="beta"
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("employees").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(1),
            Value::String("Eng".into()),
            Value::String("beta".into()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    let snap2 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // Snapshot 1 index-only scan: MUST receive "alpha", NEVER "beta"
    {
        let mut tbl = db.open_table("employees").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(201),
            Snapshot::new(SnapshotId(1), snap1),
        )));
        let rows = tbl
            .index_only_scan(
                "idx_emp_cov",
                &treedb::query::optimizer::SecondaryScanType::PointLookup(Value::String(
                    "Eng".into(),
                )),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values[2],
            Value::String("alpha".into()),
            "Older snapshot must receive alpha, not newer covering payload"
        );
    }

    // Snapshot 2 index-only scan: MUST receive "beta"
    {
        let mut tbl = db.open_table("employees").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(202),
            Snapshot::new(SnapshotId(2), snap2),
        )));
        let rows = tbl
            .index_only_scan(
                "idx_emp_cov",
                &treedb::query::optimizer::SecondaryScanType::PointLookup(Value::String(
                    "Eng".into(),
                )),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[2], Value::String("beta".into()));
    }
}

#[test]
fn test_overflow_record_historical_preservation_across_updates() {
    let (mut db, _dir) = create_test_db("overflow_mvcc_preservation");

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("data", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap();
    db.create_table("blobs", schema).unwrap();

    let payload_v1 = vec![0x41u8; 8000]; // Multi-page overflow record
    let payload_v2 = vec![0x42u8; 9000]; // Another multi-page overflow record

    // V1: insert
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("blobs").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::Bytes(payload_v1.clone()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    let s1 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // V2: update
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("blobs").unwrap();
        tbl.update(&Row::new(vec![
            Value::Int64(10),
            Value::Bytes(payload_v2.clone()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    let s2 = CommitSeq(db.buffer_pool().durable_max_commit_seq());

    // S1 reads V1: full 8000 bytes intact
    {
        let mut tbl = db.open_table("blobs").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(301),
            Snapshot::new(SnapshotId(1), s1),
        )));
        let row = tbl.get(&Value::Int64(10)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::Bytes(payload_v1));
    }

    // S2 reads V2: full 9000 bytes intact
    {
        let mut tbl = db.open_table("blobs").unwrap();
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(302),
            Snapshot::new(SnapshotId(2), s2),
        )));
        let row = tbl.get(&Value::Int64(10)).unwrap().unwrap();
        assert_eq!(row.values[1], Value::Bytes(payload_v2));
    }
}
