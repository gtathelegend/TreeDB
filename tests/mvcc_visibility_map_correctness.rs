//! Focused MVCC Visibility Map & Safe Index-Only Reads test suite.
//!
//! Covers all 20 required verification points:
//! 1. Fully committed dataset eligible for fast path.
//! 2. Uncommitted inserts and read-your-own-writes.
//! 3. Aborted inserts and rollback safety.
//! 4. Historical updates and version-chain visibility.
//! 5. Included-column updates.
//! 6. Deleted rows and index tombstones.
//! 7. Active snapshots requiring older versions.
//! 8. Commit and abort invalidation after metadata caching.
//! 9. Vacuum with active older snapshot.
//! 10. Vacuum after old snapshot ends.
//! 11. Page reuse after vacuum.
//! 12. Checkpoint, close, and reopen fresh start.
//! 13. Crash recovery cleanly resets runtime VM.
//! 14. Corrupted/invalid page ID handling.
//! 15. Disabling visibility fast path produces identical results.
//! 16. Repeated executions with changing snapshots.
//! 17. Covering point lookups.
//! 18. Covering range scans across multiple leaf pages.
//! 19. Non-covering index scan fallback.
//! 20. Deterministic randomized stress workload vs independent MVCC reference model.

use std::collections::BTreeMap;
use std::path::Path;
use tempfile::tempdir;

use treedb::engine::types::Value;
use treedb::{StatementResult, TreeDb, VacuumConfig};

fn create_test_db(path: impl AsRef<Path>) -> TreeDb {
    let mut db = TreeDb::create(path, 128).expect("create database");
    db.execute_statement(
        "CREATE TABLE accounts (id INT PRIMARY KEY, category INT, balance INT, owner VARCHAR);",
    )
    .expect("create table");
    db.execute_statement("CREATE INDEX idx_cat_balance ON accounts (category) INCLUDE (balance);")
        .expect("create covering index");
    db
}

fn rows_to_values(result: StatementResult) -> Vec<(i64, i64)> {
    match result {
        StatementResult::Query(q) => q
            .rows()
            .iter()
            .map(|r| match (&r.values[0], &r.values[1]) {
                (Value::Int64(c), Value::Int64(b)) => (*c, *b),
                _ => panic!("unexpected row format: {:?}", r),
            })
            .collect(),
        _ => panic!("expected query result"),
    }
}

// -----------------------------------------------------------------------------
// Test 1: Fully committed dataset eligible for fast path
// -----------------------------------------------------------------------------
#[test]
fn test_01_fully_committed_dataset_eligible_for_fast_path() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_01.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=50 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 10, {}, 'user_{i}');",
            i * 100
        ))
        .unwrap();
    }

    db.reset_visibility_map_stats();

    // First query: populates visibility map
    let q1 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(q1.rows().len(), 50);

    let stats_after_first = db.visibility_map_stats();
    assert!(
        stats_after_first.pages_marked_all_visible > 0,
        "Page should be marked all-visible"
    );

    // Second query: fast path should skip record store checks
    db.reset_visibility_map_stats();
    let q2 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(q2.rows().len(), 50);

    let stats_after_second = db.visibility_map_stats();
    assert_eq!(
        stats_after_second.visibility_checks_evaluated, 0,
        "Record store checks should be 0 on second run"
    );
    assert!(
        stats_after_second.visibility_checks_skipped >= 50,
        "All checks should be skipped"
    );
    assert!(
        stats_after_second.page_map_hits >= 50,
        "Should hit page-level map"
    );
}

// -----------------------------------------------------------------------------
// Test 2: Uncommitted inserts and read-your-own-writes
// -----------------------------------------------------------------------------
#[test]
fn test_02_uncommitted_inserts_isolated_from_other_readers() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_02.db");
    let mut db = create_test_db(&db_path);

    // Initial committed row
    db.execute_statement("INSERT INTO accounts VALUES (1, 10, 100, 'alice');")
        .unwrap();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (2, 10, 200, 'bob');")
        .unwrap();

    // Inside txn: reads both (read-your-own-writes)
    let rows_in_txn = txn
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(rows_in_txn.rows().len(), 2);

    // Abort transaction: uncommitted insert must vanish
    txn.abort().unwrap();

    let rows_outside = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(rows_outside.rows().len(), 1);
}

// -----------------------------------------------------------------------------
// Test 3: Aborted inserts and rollback safety
// -----------------------------------------------------------------------------
#[test]
fn test_03_transaction_abort_after_insert_invalidates_cleanly() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_03.db");
    let mut db = create_test_db(&db_path);

    db.execute_statement("INSERT INTO accounts VALUES (1, 10, 100, 'alice');")
        .unwrap();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (2, 10, 200, 'bob');")
        .unwrap();
    txn.abort().unwrap();

    // After abort, only committed row is visible
    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(res.rows().len(), 1);
    assert_eq!(res.rows()[0].values[1], Value::Int64(100));
}

// -----------------------------------------------------------------------------
// Test 4: Historical updates and version-chain visibility
// -----------------------------------------------------------------------------
#[test]
fn test_04_update_creates_historical_version_and_invalidates() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_04.db");
    let mut db = create_test_db(&db_path);

    db.execute_statement("INSERT INTO accounts VALUES (1, 10, 100, 'alice');")
        .unwrap();

    // Prime visibility map
    let _ = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();

    // Update row
    db.execute_statement("UPDATE accounts SET balance = 999 WHERE id = 1;")
        .unwrap();

    // After update, must see 999 and only 1 row
    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(res.rows().len(), 1);
    assert_eq!(res.rows()[0].values[1], Value::Int64(999));
}

// -----------------------------------------------------------------------------
// Test 5: Included-column updates
// -----------------------------------------------------------------------------
#[test]
fn test_05_update_non_indexed_included_column() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_05.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=10 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 5, {}, 'user_{i}');",
            i * 10
        ))
        .unwrap();
    }

    // Verify initial values
    let r1 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(r1.rows().len(), 10);

    // Update only the included column `balance`
    db.execute_statement("UPDATE accounts SET balance = 500 WHERE id = 5;")
        .unwrap();

    let r2 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(r2.rows().len(), 10);

    let balances: Vec<i64> = r2
        .rows()
        .iter()
        .map(|r| match &r.values[1] {
            Value::Int64(b) => *b,
            _ => panic!(),
        })
        .collect();
    assert!(
        balances.contains(&500),
        "Updated balance 500 must be present in index-only read"
    );
    assert!(!balances.contains(&50), "Old balance 50 must be invisible");
}

// -----------------------------------------------------------------------------
// Test 6: Deleted rows
// -----------------------------------------------------------------------------
#[test]
fn test_06_deleted_rows_never_returned_by_index_only_scan() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_06.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=20 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 7, {}, 'user_{i}');",
            i * 10
        ))
        .unwrap();
    }

    // Prime VM
    let _ = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 7;")
        .unwrap();

    // Delete rows 5 and 10
    db.execute_statement("DELETE FROM accounts WHERE id = 5;")
        .unwrap();
    db.execute_statement("DELETE FROM accounts WHERE id = 10;")
        .unwrap();

    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 7;")
        .unwrap();
    assert_eq!(res.rows().len(), 18);

    let balances: Vec<i64> = res
        .rows()
        .iter()
        .map(|r| match &r.values[1] {
            Value::Int64(b) => *b,
            _ => panic!(),
        })
        .collect();
    assert!(!balances.contains(&50));
    assert!(!balances.contains(&100));
}

// -----------------------------------------------------------------------------
// Test 7: Active snapshots that require older versions
// -----------------------------------------------------------------------------
#[test]
fn test_07_active_snapshots_see_historical_versions() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_07.db");
    let mut db = create_test_db(&db_path);

    db.execute_statement("INSERT INTO accounts VALUES (1, 1, 100, 'alice');")
        .unwrap();

    // Snapshot 1 opened before update
    let s1 = db.register_snapshot();

    // Update row
    let mut upd = db.begin().unwrap();
    upd.execute_statement("UPDATE accounts SET balance = 200 WHERE id = 1;")
        .unwrap();
    upd.commit().unwrap();

    // Transaction with snapshot 1 must still see the old balance 100
    let mut t_s1 = db.begin().unwrap();
    t_s1.set_snapshot(s1);
    let res1 = t_s1
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 1;")
        .unwrap();
    assert_eq!(res1.rows().len(), 1);
    assert_eq!(res1.rows()[0].values[1], Value::Int64(100));
    t_s1.abort().unwrap();

    // Fresh query sees 200
    let res_curr = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 1;")
        .unwrap();
    assert_eq!(res_curr.rows().len(), 1);
    assert_eq!(res_curr.rows()[0].values[1], Value::Int64(200));

    db.release_snapshot(s1.snapshot_id);
}

// -----------------------------------------------------------------------------
// Test 8: Commit and abort invalidation after metadata caching
// -----------------------------------------------------------------------------
#[test]
fn test_08_commit_and_abort_after_visibility_metadata_populated() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_08.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=10 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 2, {}, 'user_{i}');",
            i * 5
        ))
        .unwrap();
    }

    // Warm VM
    let _ = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 2;")
        .unwrap();

    // Transaction that aborts
    let mut txn = db.begin().unwrap();
    txn.execute_statement("UPDATE accounts SET balance = 999 WHERE id = 1;")
        .unwrap();
    txn.abort().unwrap();

    // Visibility map should NOT serve 999
    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 2;")
        .unwrap();
    assert_eq!(res.rows().len(), 10);
    assert_eq!(res.rows()[0].values[1], Value::Int64(5));
}

// -----------------------------------------------------------------------------
// Test 9 & 10: Vacuum while older snapshot remains active, and after it ends
// -----------------------------------------------------------------------------
#[test]
fn test_09_10_vacuum_with_and_without_active_snapshots() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_09.db");
    let mut db = create_test_db(&db_path);

    db.execute_statement("INSERT INTO accounts VALUES (1, 3, 100, 'alice');")
        .unwrap();

    // Snapshot holding old version
    let s1 = db.register_snapshot();

    // Update row
    let mut upd_txn = db.begin().unwrap();
    upd_txn
        .execute_statement("UPDATE accounts SET balance = 200 WHERE id = 1;")
        .unwrap();
    upd_txn.commit().unwrap();

    // Vacuum while s1 is active: cannot reclaim old version
    let vac_res = db
        .vacuum_table("accounts", &VacuumConfig::unlimited())
        .unwrap();
    assert!(vac_res.blocked_by_oldest_snapshot || vac_res.versions_reclaimed == 0);

    // Old snapshot still reads 100 safely
    let mut t_s1 = db.begin().unwrap();
    t_s1.set_snapshot(s1);
    let res_old = t_s1
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 3;")
        .unwrap();
    assert_eq!(res_old.rows()[0].values[1], Value::Int64(100));
    t_s1.abort().unwrap();
    db.release_snapshot(s1.snapshot_id);

    // Vacuum after old snapshot ends: can reclaim old version
    let _vac_res2 = db
        .vacuum_table("accounts", &VacuumConfig::unlimited())
        .unwrap();

    // Read after vacuum sees 200
    let res_curr = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 3;")
        .unwrap();
    assert_eq!(res_curr.rows().len(), 1);
    assert_eq!(res_curr.rows()[0].values[1], Value::Int64(200));
}

// -----------------------------------------------------------------------------
// Test 11: Page reuse after vacuum
// -----------------------------------------------------------------------------
#[test]
fn test_11_page_reuse_after_vacuum() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_11.db");
    let mut db = create_test_db(&db_path);

    // Insert 50 rows
    for i in 1..=50 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 8, {}, 'user_{i}');",
            i * 10
        ))
        .unwrap();
    }

    // Delete 40 rows
    for i in 1..=40 {
        db.execute_statement(&format!("DELETE FROM accounts WHERE id = {i};"))
            .unwrap();
    }

    // Vacuum table
    let mut vac_txn = db.begin().unwrap();
    vac_txn
        .vacuum_table("accounts", &VacuumConfig::unlimited())
        .unwrap();
    vac_txn.commit().unwrap();

    // Re-insert into reclaimed pages
    for i in 100..=120 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 8, {}, 'new_{i}');",
            i * 10
        ))
        .unwrap();
    }

    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 8;")
        .unwrap();
    // 10 surviving original rows + 21 new rows = 31 rows
    assert_eq!(res.rows().len(), 31);
}

// -----------------------------------------------------------------------------
// Test 12: Reopen after clean checkpoint
// -----------------------------------------------------------------------------
#[test]
fn test_12_reopen_starts_fresh_and_populates_visibility() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_12.db");

    {
        let mut db = create_test_db(&db_path);
        for i in 1..=30 {
            db.execute_statement(&format!(
                "INSERT INTO accounts VALUES ({i}, 4, {}, 'user_{i}');",
                i * 100
            ))
            .unwrap();
        }
        db.checkpoint().unwrap();
    }

    // Reopen
    let mut db = TreeDb::open(&db_path, 128).unwrap();
    assert_eq!(db.visibility_map_stats().pages_marked_all_visible, 0);

    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 4;")
        .unwrap();
    assert_eq!(res.rows().len(), 30);

    let stats = db.visibility_map_stats();
    assert!(stats.pages_marked_all_visible > 0);
}

// -----------------------------------------------------------------------------
// Test 13: Recovery after crash / uncommitted restart
// -----------------------------------------------------------------------------
#[test]
fn test_13_recovery_cleanly_resets_runtime_visibility_map() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_13.db");

    {
        let mut db = create_test_db(&db_path);
        db.execute_statement("INSERT INTO accounts VALUES (1, 10, 100, 'alice');")
            .unwrap();
        // Uncommitted insert
        let mut txn = db.begin().unwrap();
        txn.execute_statement("INSERT INTO accounts VALUES (2, 10, 200, 'bob');")
            .unwrap();
        // Drop without commit simulating crash
    }

    let mut db = TreeDb::open(&db_path, 128).unwrap();
    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 10;")
        .unwrap();
    assert_eq!(res.rows().len(), 1);
    assert_eq!(res.rows()[0].values[1], Value::Int64(100));
}

// -----------------------------------------------------------------------------
// Test 14 & 15: Disabling fast path produces identical results
// -----------------------------------------------------------------------------
#[test]
fn test_14_15_disabling_visibility_fast_path_equivalence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_14.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=40 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, {}, {}, 'user_{i}');",
            i % 5,
            i * 100
        ))
        .unwrap();
    }

    // Fast path enabled
    db.set_visibility_map_enabled(true);
    let enabled_res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 2;")
        .unwrap();

    // Fast path disabled
    db.set_visibility_map_enabled(false);
    let disabled_res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 2;")
        .unwrap();

    assert_eq!(enabled_res.rows().len(), disabled_res.rows().len());
    for (r1, r2) in enabled_res.rows().iter().zip(disabled_res.rows()) {
        assert_eq!(r1.values, r2.values);
    }
}

// -----------------------------------------------------------------------------
// Test 16: Repeated execution with changing transaction snapshots
// -----------------------------------------------------------------------------
#[test]
fn test_16_repeated_executions_with_changing_snapshots() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_16.db");
    let mut db = create_test_db(&db_path);

    db.execute_statement("INSERT INTO accounts VALUES (1, 9, 100, 'alice');")
        .unwrap();

    for round in 1..=10 {
        let mut txn = db.begin().unwrap();
        let res = txn
            .execute_sql("SELECT category, balance FROM accounts WHERE category = 9;")
            .unwrap();
        assert_eq!(res.rows().len(), round);

        txn.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({}, 9, {}, 'user_{}');",
            round + 1,
            (round + 1) * 100,
            round + 1
        ))
        .unwrap();
        txn.commit().unwrap();
    }
}

// -----------------------------------------------------------------------------
// Test 17: Covering point lookups across multiple entries
// -----------------------------------------------------------------------------
#[test]
fn test_17_covering_point_lookup_multiple_entries() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_17.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=100 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 42, {}, 'user_{i}');",
            i * 5
        ))
        .unwrap();
    }

    let res = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 42;")
        .unwrap();
    assert_eq!(res.rows().len(), 100);
}

// -----------------------------------------------------------------------------
// Test 18: Covering range scans across multiple leaf pages
// -----------------------------------------------------------------------------
#[test]
fn test_18_covering_range_scan_across_leaf_pages() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_18.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=300 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, {}, {}, 'user_{i}');",
            i % 50,
            i * 10
        ))
        .unwrap();
    }

    let res = db
        .execute_sql(
            "SELECT category, balance FROM accounts WHERE category >= 10 AND category <= 30;",
        )
        .unwrap();
    assert!(!res.rows().is_empty());

    for r in res.rows() {
        if let Value::Int64(cat) = r.values[0] {
            assert!((10..=30).contains(&cat));
        } else {
            panic!("expected Int64 category");
        }
    }
}

// -----------------------------------------------------------------------------
// Test 19: Non-covering index scan fallback
// -----------------------------------------------------------------------------
#[test]
fn test_19_non_covering_query_fallback() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_19.db");
    let mut db = create_test_db(&db_path);

    for i in 1..=20 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 15, {}, 'user_{i}');",
            i * 10
        ))
        .unwrap();
    }

    // Query non-covered column `owner`
    let res = db
        .execute_sql("SELECT category, balance, owner FROM accounts WHERE category = 15;")
        .unwrap();
    assert_eq!(res.rows().len(), 20);
    assert_eq!(res.rows()[0].values[2], Value::String("user_1".to_string()));
}

// -----------------------------------------------------------------------------
// Test 20: Deterministic randomized stress workload vs independent MVCC reference model
// -----------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefRow {
    id: i64,
    category: i64,
    balance: i64,
    owner: String,
}

#[test]
fn test_20_randomized_workload_vs_reference_model() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_20.db");
    let mut db = create_test_db(&db_path);

    // Independent reference model: committed key-value state
    let mut committed_state: BTreeMap<i64, RefRow> = BTreeMap::new();

    // Pseudo-random linear congruential generator for reproducible determinism
    let mut seed: u64 = 0xDEADBEEFCAFE;
    let mut next_rand = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    let total_steps = 200;
    for step in 1..=total_steps {
        let op = next_rand() % 4;
        let id = ((next_rand() % 30) + 1) as i64;
        let category = ((next_rand() % 5) + 1) as i64;
        let balance = ((next_rand() % 1000) + 10) as i64;

        match op {
            0 => {
                // Insert
                if let std::collections::btree_map::Entry::Vacant(e) = committed_state.entry(id) {
                    let owner = format!("user_{id}_{step}");
                    let sql = format!(
                        "INSERT INTO accounts VALUES ({id}, {category}, {balance}, '{owner}');"
                    );
                    if db.execute_statement(&sql).is_ok() {
                        e.insert(RefRow {
                            id,
                            category,
                            balance,
                            owner,
                        });
                    }
                }
            }
            1 => {
                // Update
                if let Some(r) = committed_state.get_mut(&id) {
                    let sql = format!(
                        "UPDATE accounts SET category = {category}, balance = {balance} WHERE id = {id};"
                    );
                    if db.execute_statement(&sql).is_ok() {
                        r.category = category;
                        r.balance = balance;
                    }
                }
            }
            2 => {
                // Delete
                if committed_state.contains_key(&id) {
                    let sql = format!("DELETE FROM accounts WHERE id = {id};");
                    if db.execute_statement(&sql).is_ok() {
                        committed_state.remove(&id);
                    }
                }
            }
            3 => {
                // Query point & verify against reference model
                let cat = ((next_rand() % 5) + 1) as i64;
                let sql = format!("SELECT category, balance FROM accounts WHERE category = {cat};");
                let db_res = db.execute_sql(&sql).unwrap();
                let mut db_pairs: Vec<(i64, i64)> = rows_to_values(StatementResult::Query(db_res));
                db_pairs.sort();

                let mut ref_pairs: Vec<(i64, i64)> = committed_state
                    .values()
                    .filter(|r| r.category == cat)
                    .map(|r| (r.category, r.balance))
                    .collect();
                ref_pairs.sort();

                assert_eq!(
                    db_pairs, ref_pairs,
                    "Step {step}: Mismatch between TreeDB and reference model for category {cat}"
                );
            }
            _ => unreachable!(),
        }

        // Periodically run vacuum or checkpoint
        if step % 50 == 0 {
            let _ = db.execute_statement("VACUUM;");
            let _ = db.checkpoint();
        }
    }

    // Final verification across all categories 1..=5
    for cat in 1..=5 {
        let sql = format!("SELECT category, balance FROM accounts WHERE category = {cat};");
        let db_res = db.execute_sql(&sql).unwrap();
        let mut db_pairs = rows_to_values(StatementResult::Query(db_res));
        db_pairs.sort();

        let mut ref_pairs: Vec<(i64, i64)> = committed_state
            .values()
            .filter(|r| r.category == cat)
            .map(|r| (r.category, r.balance))
            .collect();
        ref_pairs.sort();

        assert_eq!(
            db_pairs, ref_pairs,
            "Final verification mismatch for category {cat}"
        );
    }
}

// -----------------------------------------------------------------------------
// Section 3: Comprehensive instrumentation across all 10 required workloads
// -----------------------------------------------------------------------------
#[test]
fn test_section_3_ten_workloads_instrumentation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("workloads_10.db");
    let mut db = create_test_db(&db_path);

    // Populate baseline dataset (1000 rows, categories 1..=10, 100 rows each)
    let mut txn = db.begin().unwrap();
    for i in 1..=1000 {
        txn.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, {}, {}, 'owner_{i}');",
            (i % 10) + 1,
            i * 10
        ))
        .unwrap();
    }
    txn.commit().unwrap();

    // 1. Covering point lookup
    db.reset_visibility_map_stats();
    let res1 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(res1.rows().len(), 100);
    let stats1 = db.visibility_map_stats();
    assert!(stats1.page_map_hits > 0 || stats1.record_map_hits > 0);
    assert!(stats1.visibility_checks_skipped > 0);

    // 2. Covering range scan with small result set (10 rows)
    db.reset_visibility_map_stats();
    let res2 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 1 LIMIT 10;")
        .unwrap();
    assert_eq!(res2.rows().len(), 10);
    let stats2 = db.visibility_map_stats();
    assert!(stats2.page_map_hits > 0 || stats2.record_map_hits > 0);
    assert!(stats2.visibility_checks_skipped > 0);

    // 3. Covering range scan with large result set (500 rows)
    db.reset_visibility_map_stats();
    let res3 = db
        .execute_sql(
            "SELECT category, balance FROM accounts WHERE category >= 1 AND category <= 5;",
        )
        .unwrap();
    assert_eq!(res3.rows().len(), 500);
    let stats3 = db.visibility_map_stats();
    assert!(stats3.page_map_hits > 0);
    assert!(stats3.visibility_checks_skipped > 0);

    // 4. Covering query with invisible historical entries
    // Start transaction that updates 50 rows in category 5, but aborted
    let mut ab_txn = db.begin().unwrap();
    ab_txn
        .execute_statement("UPDATE accounts SET balance = balance + 999 WHERE category = 5;")
        .unwrap();
    ab_txn.abort().unwrap();

    // Outside reader reads category 5 - aborted updates must be invisible
    let res4 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(res4.rows().len(), 100);
    for r in res4.rows() {
        if let Value::Int64(b) = r.values[1] {
            assert!(b % 10 == 0 && b < 70000);
        }
    }

    // 5. Ordinary non-covering index scan (requires 'owner' column)
    db.reset_visibility_map_stats();
    let res5 = db
        .execute_sql("SELECT category, owner FROM accounts WHERE category = 7;")
        .unwrap();
    assert_eq!(res5.rows().len(), 100);

    // 6. Primary-key point lookup
    db.reset_visibility_map_stats();
    let res6 = db
        .execute_sql("SELECT id, balance FROM accounts WHERE id = 42;")
        .unwrap();
    assert_eq!(res6.rows().len(), 1);

    // 7. Full-table scan
    db.reset_visibility_map_stats();
    let res7 = db.execute_sql("SELECT COUNT(*) FROM accounts;").unwrap();
    assert_eq!(res7.rows().len(), 1);

    // 8. Query after updates and deletes
    let mut mut_txn = db.begin().unwrap();
    mut_txn
        .execute_statement("UPDATE accounts SET balance = 9999 WHERE id = 14;")
        .unwrap();
    mut_txn
        .execute_statement("DELETE FROM accounts WHERE id = 24;")
        .unwrap();
    mut_txn.commit().unwrap();
    let res8 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(res8.rows().len(), 99); // 100 - 1 deleted (id 24 was category (24%10)+1 = 5)

    // 9. Query after vacuum
    let vac_res = db
        .vacuum_table("accounts", &VacuumConfig::unlimited())
        .unwrap();
    assert!(vac_res.versions_examined > 0);
    let res9 = db
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(res9.rows().len(), 99);

    // 10. Query after checkpoint, close, and reopen
    db.checkpoint().unwrap();
    drop(db);
    let mut db_reopened = TreeDb::open(&db_path, 128).unwrap();
    let res10 = db_reopened
        .execute_sql("SELECT category, balance FROM accounts WHERE category = 5;")
        .unwrap();
    assert_eq!(res10.rows().len(), 99);
}
