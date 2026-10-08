use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::query::execution::{
    ExecutionContext, ExecutionOperator, OperatorLifecycleState, PhysicalScanOperator,
};
use treedb::query::metrics::{OperatorKind, OperatorMetrics};
use treedb::query::optimizer::AccessPath;
use treedb::query::reference::{assert_differential_equivalence, DifferentialFailureReport};
use treedb::query::validation::{PhysicalPlanValidator, PlanValidationError};
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_pathological_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_pathological_{name}.treedb.wal"));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
        Self { db_path, wal_path }
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
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

// -----------------------------------------------------------------------------
// 1. Data Distribution Families (Section 7)
// -----------------------------------------------------------------------------

#[test]
fn test_pathological_data_distributions() {
    let ctx = TestContext::new("distributions");
    let mut db = ctx.db();

    // Tables covering: Uniform, Constant, Zipf-like, Alternating, Sparse, Anti-correlated, Perfectly correlated
    db.execute_statement(
        "CREATE TABLE dist_test (
            id UINT64 PRIMARY KEY,
            val_uniform INT64,
            val_constant INT64,
            val_zipf INT64,
            val_alternating STRING,
            val_sparse INT64,
            val_anticorr_a INT64,
            val_anticorr_b INT64,
            val_corr_a INT64,
            val_corr_b INT64
        )",
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let n = 200;
    for i in 1..=n {
        let uniform = i * 17 % 1000;
        let constant = 42;
        // Zipf-like: 1 is very common, 2 less common, etc.
        let zipf = if i % 2 == 0 {
            1
        } else if i % 4 == 1 {
            2
        } else if i % 8 == 3 {
            3
        } else {
            i
        };
        let alternating = if i % 2 == 0 { "'A'" } else { "'B'" };
        let sparse = if i % 10 == 0 {
            format!("{i}")
        } else {
            "NULL".to_string()
        };
        let anticorr_a = i;
        let anticorr_b = 1000 - i;
        let corr_a = i;
        let corr_b = i;

        let sql = format!(
            "INSERT INTO dist_test VALUES ({i}, {uniform}, {constant}, {zipf}, {alternating}, {sparse}, {anticorr_a}, {anticorr_b}, {corr_a}, {corr_b})"
        );
        txn.execute_statement(&sql).unwrap();
    }
    txn.commit().unwrap();

    let queries = [
        // Constant distribution queries
        "SELECT val_constant, COUNT(*), MIN(id), MAX(id) FROM dist_test GROUP BY val_constant",
        // Uniform distribution queries
        "SELECT val_uniform, COUNT(*) FROM dist_test WHERE val_uniform > 500 GROUP BY val_uniform ORDER BY val_uniform LIMIT 10",
        // Zipf distribution queries (skewed keys)
        "SELECT val_zipf, COUNT(*) AS cnt FROM dist_test GROUP BY val_zipf ORDER BY cnt DESC LIMIT 5",
        // Alternating distribution
        "SELECT val_alternating, COUNT(*), AVG(val_uniform) FROM dist_test GROUP BY val_alternating",
        // Sparse distribution (mostly NULLs)
        "SELECT COUNT(*), COUNT(val_sparse), SUM(val_sparse), MIN(val_sparse) FROM dist_test",
        "SELECT id, val_sparse FROM dist_test WHERE val_sparse IS NOT NULL ORDER BY id",
        // Anti-correlated distribution
        "SELECT id, val_anticorr_a, val_anticorr_b FROM dist_test WHERE val_anticorr_a > 100 AND val_anticorr_b > 850 ORDER BY id",
        // Perfectly correlated distribution
        "SELECT id, val_corr_a, val_corr_b FROM dist_test WHERE val_corr_a = val_corr_b ORDER BY id LIMIT 20",
        // Multi-column grouping with skewed/constant columns
        "SELECT val_constant, val_alternating, COUNT(*) FROM dist_test GROUP BY val_constant, val_alternating ORDER BY val_alternating",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        let require_order = sql.contains("ORDER BY");
        assert_differential_equivalence(&opt, &ref_res, require_order);
    }
}

// -----------------------------------------------------------------------------
// 2. Duplicate-Key Pathologies (Section 27)
// -----------------------------------------------------------------------------

#[test]
fn test_pathological_duplicate_key_runs() {
    let ctx = TestContext::new("duplicate_runs");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE dup_left (id UINT64 PRIMARY KEY, k INT64, v STRING)")
        .unwrap();
    db.execute_statement("CREATE TABLE dup_right (id UINT64 PRIMARY KEY, k INT64, v STRING)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    // 500 rows with same key 100 in left, and 20 rows with key 100 in right -> 10,000 combinations
    let left_dup = 500;
    for i in 1..=left_dup {
        let sql = format!("INSERT INTO dup_left VALUES ({i}, 100, 'L{i}')");
        txn.execute_statement(&sql).unwrap();
    }
    // non-matching key
    txn.execute_statement("INSERT INTO dup_left VALUES (9999, 200, 'L_other')")
        .unwrap();

    let right_dup = 20;
    for i in 1..=right_dup {
        let sql = format!("INSERT INTO dup_right VALUES ({i}, 100, 'R{i}')");
        txn.execute_statement(&sql).unwrap();
    }
    txn.execute_statement("INSERT INTO dup_right VALUES (9999, 300, 'R_other')")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // Exact multiplicity join: 500 * 20 = 10,000 rows
        "SELECT COUNT(*) FROM dup_left l INNER JOIN dup_right r ON l.k = r.k",
        // Grouping over duplicate keys
        "SELECT k, COUNT(*), MIN(v), MAX(v) FROM dup_left GROUP BY k",
        // Left outer join with huge duplicate run
        "SELECT COUNT(*) FROM dup_left l LEFT JOIN dup_right r ON l.k = r.k",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 3. Row-Goal Pathological Boundaries (Section 26)
// -----------------------------------------------------------------------------

#[test]
fn test_pathological_row_goal_boundary() {
    let ctx = TestContext::new("row_goals");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE row_goal_test (id UINT64 PRIMARY KEY, val INT64, flag INT64)",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_rg_val ON row_goal_test (val)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let total = 2000;
    for i in 1..=total {
        // Only row 1999 has flag = 1; all others have flag = 0
        let flag = if i == 1999 { 1 } else { 0 };
        let sql = format!("INSERT INTO row_goal_test VALUES ({i}, {i}, {flag})");
        txn.execute_statement(&sql).unwrap();
    }
    txn.commit().unwrap();

    let queries = [
        // 1 row qualifies out of 2000, ORDER BY val LIMIT 1
        "SELECT id, val FROM row_goal_test WHERE flag = 1 ORDER BY val ASC LIMIT 1",
        // 1999 rows qualify out of 2000, ORDER BY val LIMIT 1
        "SELECT id, val FROM row_goal_test WHERE flag = 0 ORDER BY val ASC LIMIT 1",
        // LIMIT beyond matching rows
        "SELECT id, val FROM row_goal_test WHERE flag = 1 ORDER BY val ASC LIMIT 10",
        // OFFSET + LIMIT boundary
        "SELECT id, val FROM row_goal_test WHERE flag = 0 ORDER BY val ASC LIMIT 5 OFFSET 1990",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, true);
    }
}

// -----------------------------------------------------------------------------
// 4. Recursive CTE Stress & Cycle Rejection (Sections 21, 22)
// -----------------------------------------------------------------------------

#[test]
fn test_pathological_recursive_cte_limits_and_cycles() {
    let ctx = TestContext::new("cte_limits");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE seeds (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO seeds VALUES (1, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // 1. Recursive CTE iteration boundary (small count)
    let sql_small = "WITH RECURSIVE cnt(x) AS (
        SELECT val FROM seeds WHERE id = 1
        UNION ALL
        SELECT x + 1 FROM cnt WHERE x < 5
    )
    SELECT x FROM cnt ORDER BY x";
    let opt = txn.execute_sql(sql_small).unwrap();
    let ref_res = txn.execute_sql_reference(sql_small).unwrap();
    assert_differential_equivalence(&opt, &ref_res, true);

    // 2. Recursive CTE with termination predicate
    let sql_step = "WITH RECURSIVE cnt2(x) AS (
        SELECT val FROM seeds WHERE id = 1
        UNION ALL
        SELECT x * 2 FROM cnt2 WHERE x < 16
    )
    SELECT x FROM cnt2 ORDER BY x";
    let opt = txn.execute_sql(sql_step).unwrap();
    let ref_res = txn.execute_sql_reference(sql_step).unwrap();
    assert_differential_equivalence(&opt, &ref_res, true);

    // 3. Recursive cycle / self-reference error handling (must not panic)
    // Query that generates infinite loop must be caught by recursive iteration limit or row limit
    let sql_infinite = "WITH RECURSIVE inf(x) AS (
        SELECT val FROM seeds WHERE id = 1
        UNION ALL
        SELECT x FROM inf
    )
    SELECT x FROM inf";
    let res = txn.execute_sql(sql_infinite);
    assert!(
        res.is_err(),
        "Infinite recursion must return structured error, not loop forever"
    );
}

// -----------------------------------------------------------------------------
// 5. Physical Plan Validation Fuzzing (Section 40)
// -----------------------------------------------------------------------------

#[test]
fn test_fuzzing_physical_plan_validation() {
    let ctx = TestContext::new("plan_val_fuzz");
    let mut db = ctx.db();

    let mut tx = db.begin_transaction().unwrap();
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a BIGINT, b VARCHAR);")
        .unwrap();
    let val_ctx = tx.create_validation_context().unwrap();
    let plan = tx.plan_sql("SELECT id, a FROM t;").unwrap();

    // 1. Valid baseline plan passes validation
    let valid_res = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(valid_res.is_ok(), "Valid plan must pass validation");

    // 2. Fuzzing: Required column missing from table
    let mut mutated1 = plan.clone();
    mutated1
        .base_table
        .required_columns
        .push("nonexistent_column".to_string());
    let err1 = PhysicalPlanValidator::validate(&mutated1, &val_ctx);
    assert!(
        err1.is_err(),
        "Nonexistent required column must fail validation"
    );
    assert!(matches!(
        err1.unwrap_err(),
        PlanValidationError::RequiredColumnsViolation { .. }
    ));

    // 3. Fuzzing: Missing projected column
    let mut mutated2 = plan.clone();
    mutated2.projections.push((
        treedb::query::expr::Expr::Column("ghost_col".to_string()),
        None,
    ));
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx);
    assert!(err2.is_err(), "Ghost projected column must fail validation");
    assert!(matches!(
        err2.unwrap_err(),
        PlanValidationError::MissingColumn { .. }
    ));

    // 4. Fuzzing: Invalid access path (missing index)
    let mut mutated3 = plan.clone();
    mutated3.base_table.access_path = AccessPath::IndexOnlyScan {
        index_name: "ghost_index".to_string(),
        index_id: treedb::IndexId(9999),
        scan_type: treedb::query::optimizer::SecondaryScanType::PointLookup(Value::Int64(1)),
        covered_columns: vec!["a".to_string()],
    };
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx);
    assert!(err3.is_err(), "Ghost index must fail validation");

    tx.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 6. Execution State Lifecycle Fuzzing (Section 41)
// -----------------------------------------------------------------------------

#[test]
fn test_fuzzing_operator_lifecycle_state_transitions() {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "mock".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(1.0),
        Some(10.0),
    );

    let rows = vec![Row::new(vec![Value::UInt64(1), Value::Int64(10)])];
    let mut op =
        PhysicalScanOperator::new("mock".to_string(), schema, rows, metrics, None, None, false);

    assert_eq!(op.state(), OperatorLifecycleState::Created);

    // Fuzz 1: next() called before open() must return structured lifecycle error, not panic
    let next_res = op.next(&mut ctx);
    assert!(
        next_res.is_err(),
        "next() before open() must fail deterministically"
    );

    // Fuzz 2: open() transitions to Opened
    let open_res = op.open(&mut ctx);
    assert!(open_res.is_ok(), "open() must succeed");
    assert_eq!(op.state(), OperatorLifecycleState::Opened);

    // Fuzz 3: close() transitions to Closed
    let close_res = op.close(&mut ctx);
    assert!(close_res.is_ok(), "close() must succeed");
    assert_eq!(op.state(), OperatorLifecycleState::Closed);

    // Fuzz 4: next() called after close() must return structured lifecycle error, not panic
    let next_after_close = op.next(&mut ctx);
    assert!(
        next_after_close.is_err(),
        "next() after close() must fail deterministically"
    );

    // Fuzz 5: repeated close() must be idempotent and succeed safely
    let double_close = op.close(&mut ctx);
    assert!(double_close.is_ok(), "idempotent close() must succeed");
}

// -----------------------------------------------------------------------------
// 7. Long-Running Deterministic Randomized Transactional Stress (Section 43)
// -----------------------------------------------------------------------------

#[test]
fn test_long_running_randomized_transactional_stress() {
    let ctx = TestContext::new("randomized_stress_long");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE store (
            id UINT64 PRIMARY KEY,
            k INT64,
            category STRING,
            active BOOLEAN,
            score FLOAT64
        )",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_store_k ON store (k)")
        .unwrap();

    let seed = 424242u64;
    let mut rng = seed;
    let mut next_rng = || -> u64 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng
    };

    let operations = 100;
    let mut active_ids = Vec::new();

    for step in 1..=operations {
        let op_type = next_rng() % 5;
        let mut txn = db.begin_transaction().unwrap();

        match op_type {
            // INSERT
            0 | 1 => {
                let id = step as u64;
                let k = (next_rng() % 50) as i64;
                let cat = match next_rng() % 3 {
                    0 => "'electronics'",
                    1 => "'books'",
                    _ => "'clothing'",
                };
                let active = if next_rng() % 2 == 0 { "TRUE" } else { "FALSE" };
                let score = ((next_rng() % 1000) as f64) / 10.0;

                let sql = format!("INSERT INTO store VALUES ({id}, {k}, {cat}, {active}, {score})");
                txn.execute_statement(&sql).unwrap();
                active_ids.push(id);
                txn.commit().unwrap();
            }
            // UPDATE
            2 => {
                if !active_ids.is_empty() {
                    let target_idx = (next_rng() as usize) % active_ids.len();
                    let target_id = active_ids[target_idx];
                    let new_k = (next_rng() % 50) as i64;
                    let sql = format!("UPDATE store SET k = {new_k} WHERE id = {target_id}");
                    txn.execute_statement(&sql).unwrap();
                }
                txn.commit().unwrap();
            }
            // DELETE
            3 => {
                if !active_ids.is_empty() {
                    let target_idx = (next_rng() as usize) % active_ids.len();
                    let target_id = active_ids.swap_remove(target_idx);
                    let sql = format!("DELETE FROM store WHERE id = {target_id}");
                    txn.execute_statement(&sql).unwrap();
                }
                txn.commit().unwrap();
            }
            // SELECT and Differential Check
            _ => {
                let query_kind = next_rng() % 4;
                let sql = match query_kind {
                    0 => "SELECT COUNT(*), MIN(k), MAX(k), AVG(score) FROM store".to_string(),
                    1 => "SELECT category, COUNT(*), SUM(k) FROM store GROUP BY category ORDER BY category".to_string(),
                    2 => "SELECT id, k, score FROM store WHERE k >= 25 ORDER BY id LIMIT 5".to_string(),
                    _ => "SELECT id, k, category FROM store WHERE active = TRUE ORDER BY k ASC, id ASC LIMIT 10".to_string(),
                };

                let opt = txn.execute_sql(&sql).unwrap();
                let ref_res = txn.execute_sql_reference(&sql).unwrap();
                let require_order = sql.contains("ORDER BY");

                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    assert_differential_equivalence(&opt, &ref_res, require_order);
                })) {
                    let report = DifferentialFailureReport {
                        seed,
                        step,
                        sql: sql.clone(),
                        error_message: format!("{e:?}"),
                        chosen_strategy: Some("Optimized".to_string()),
                    };
                    panic!("Randomized differential check failed: {report}");
                }
                txn.commit().unwrap();
            }
        }
    }
}
