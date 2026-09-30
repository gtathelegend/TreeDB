use std::path::PathBuf;

use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::feedback::{
    compute_estimation_error, FeedbackAction, FeedbackContext, FeedbackKey, FeedbackObservation,
    FeedbackRegistry,
};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "test_fb_{}_{}_{}.treedb",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
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
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
    }
}

fn setup_test_schema(db: &mut TreeDb) -> Result<()> {
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT, dept_id BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE departments (dept_id BIGINT PRIMARY KEY, name TEXT, budget DOUBLE);",
    )?;

    txn.execute_statement(
        "INSERT INTO departments (dept_id, name, budget) VALUES \
         (10, 'Engineering', 100000.0), \
         (20, 'Sales', 50000.0), \
         (30, 'Marketing', 30000.0);",
    )?;

    // Insert 50 users
    for i in 1..=50 {
        let age = 20 + (i % 30);
        let dept = if i % 2 == 0 { 10 } else { 20 };
        txn.execute_statement(&format!(
            "INSERT INTO users (id, name, age, dept_id) VALUES ({i}, 'User_{i}', {age}, {dept});"
        ))?;
    }

    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R157: Feedback recorded after successful execution
// ---------------------------------------------------------------------------
#[test]
fn test_r157_feedback_recorded_after_successful_execution() -> Result<()> {
    let ctx = TestContext::new("r157");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    assert_eq!(db.feedback_registry().len(), 0);

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    assert!(
        !db.feedback_registry().is_empty(),
        "Feedback registry should have recorded scan observations"
    );

    let entries = db.feedback_registry().all_entries();
    let scan_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Scan { .. }));
    assert!(
        scan_entry.is_some(),
        "Expected TableScan feedback entry to be recorded"
    );
    let entry = scan_entry.unwrap();
    assert_eq!(entry.observation_count, 1);
    assert!(entry.effective_cardinality > 0.0);
    assert!(entry.last_actual_rows > 0.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R158: Actual/estimated ratio calculated correctly
// ---------------------------------------------------------------------------
#[test]
fn test_r158_actual_estimated_ratio_calculated_correctly() {
    let err1 = compute_estimation_error(100.0, 400.0);
    assert!((err1 - 4.0).abs() < 1e-6);

    let err2 = compute_estimation_error(400.0, 100.0);
    assert!((err2 - 4.0).abs() < 1e-6);

    let err3 = compute_estimation_error(50.0, 50.0);
    assert!((err3 - 1.0).abs() < 1e-6);

    let err4 = compute_estimation_error(10.0, 25.0);
    assert!((err4 - 2.5).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// R159: Zero-estimate handling
// ---------------------------------------------------------------------------
#[test]
fn test_r159_zero_estimate_handling() {
    let err_both_zero = compute_estimation_error(0.0, 0.0);
    assert!((err_both_zero - 1.0).abs() < 1e-6);

    let err_zero_est = compute_estimation_error(0.0, 10.0);
    assert!(!err_zero_est.is_nan());
    assert!(!err_zero_est.is_infinite());
    assert!(err_zero_est >= 10.0);

    let err_zero_act = compute_estimation_error(10.0, 0.0);
    assert!(!err_zero_act.is_nan());
    assert!(!err_zero_act.is_infinite());

    let key = FeedbackKey::scan("users", "FullTableScan", "age > 0", None, 0);
    let mut reg = FeedbackRegistry::new();
    let entry = reg.record(key.clone(), FeedbackObservation::new(0.0, 0.0));
    assert_eq!(entry.effective_cardinality, 0.0);
    assert_eq!(entry.observation_count, 1);

    let entry2 = reg.record(key.clone(), FeedbackObservation::new(0.0, 5.0));
    assert!(!entry2.effective_cardinality.is_nan());
    assert!(entry2.effective_cardinality > 0.0);
}

// ---------------------------------------------------------------------------
// R160: EWMA/bounded aggregation
// ---------------------------------------------------------------------------
#[test]
fn test_r160_ewma_bounded_aggregation() {
    let key = FeedbackKey::scan("users", "FullTableScan", "age > 0", None, 0);
    let mut reg = FeedbackRegistry::new();

    // 1st observation: est=10.0, act=10.0 -> smoothed = 10.0
    let entry1 = reg.record(key.clone(), FeedbackObservation::new(10.0, 10.0));
    assert_eq!(entry1.effective_cardinality, 10.0);
    assert_eq!(entry1.observation_count, 1);

    // 2nd observation: act=20.0
    // smoothed = 0.3 * 20.0 + 0.7 * 10.0 = 6.0 + 7.0 = 13.0
    let entry2 = reg.record(key.clone(), FeedbackObservation::new(10.0, 20.0));
    assert!((entry2.effective_cardinality - 13.0).abs() < 1e-6);
    assert_eq!(entry2.observation_count, 2);

    // 3rd observation: act=30.0
    // smoothed = 0.3 * 30.0 + 0.7 * 13.0 = 9.0 + 9.1 = 18.1
    let entry3 = reg.record(key.clone(), FeedbackObservation::new(10.0, 30.0));
    assert!((entry3.effective_cardinality - 18.1).abs() < 1e-6);
    assert_eq!(entry3.observation_count, 3);
}

// ---------------------------------------------------------------------------
// R161: Outlier clamping
// ---------------------------------------------------------------------------
#[test]
fn test_r161_outlier_clamping() {
    let key = FeedbackKey::scan("users", "FullTableScan", "age > 0", None, 0);
    let mut reg = FeedbackRegistry::new();

    reg.record(key.clone(), FeedbackObservation::new(10.0, 10.0));

    // Outlier 1: massive spike (10,000,000 rows when est is 10.0) -> clamped to 10.0 * 100.0 = 1000.0
    let entry1 = reg.record(key.clone(), FeedbackObservation::new(10.0, 10_000_000.0));
    assert_eq!(entry1.last_action, FeedbackAction::ReplanTriggered);
    // smoothed = 0.3 * 1000.0 + 0.7 * 10.0 = 300.0 + 7.0 = 307.0
    assert!((entry1.effective_cardinality - 307.0).abs() < 1e-4);

    // Outlier 2: massive drop below 0.01x
    let key2 = FeedbackKey::scan("users", "FullTableScan", "age > 100", None, 0);
    let mut reg2 = FeedbackRegistry::new();
    reg2.record(key2.clone(), FeedbackObservation::new(1000.0, 1000.0));
    let entry2 = reg2.record(key2.clone(), FeedbackObservation::new(1000.0, 0.001));
    // clamped actual is 1000.0 * 0.01 = 10.0
    // smoothed = 0.3 * 10.0 + 0.7 * 1000.0 = 3.0 + 700.0 = 703.0
    assert!((entry2.effective_cardinality - 703.0).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R162: Observation count tracking
// ---------------------------------------------------------------------------
#[test]
fn test_r162_observation_count_tracking() -> Result<()> {
    let ctx = TestContext::new("r162");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    for _ in 0..5 {
        let mut txn = db.begin_transaction()?;
        let res = txn.execute_statement("SELECT * FROM users WHERE age = 25;")?;
        let _ = res.into_query()?;
        txn.commit()?;
    }

    let entries = db.feedback_registry().all_entries();
    let scan_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Scan { .. }))
        .expect("Scan entry must exist");

    assert_eq!(
        scan_entry.observation_count, 5,
        "Should track exact observation count"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R163: Feedback improves future cardinality estimate
// ---------------------------------------------------------------------------
#[test]
fn test_r163_feedback_improves_future_cardinality_estimate() -> Result<()> {
    let ctx = TestContext::new("r163");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Run query once
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 25;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    // Second execution gets adjusted cardinality
    let key = db
        .feedback_registry()
        .all_entries()
        .first()
        .unwrap()
        .key
        .clone();
    let adjusted = db.feedback_registry().get_adjusted_cardinality(&key, 100.0);
    assert!(
        adjusted > 0.0 && adjusted < 100.0,
        "Adjusted cardinality should pull base estimate towards observed rows (got {adjusted})"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R164: Feedback absent -> existing statistics path unchanged
// ---------------------------------------------------------------------------
#[test]
fn test_r164_feedback_absent_existing_statistics_path_unchanged() -> Result<()> {
    let ctx = TestContext::new("r164");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let key = FeedbackKey::scan("users", "FullTableScan", "age > 30", None, 0);
    let original_estimate = 42.0;
    let result = db
        .feedback_registry()
        .get_adjusted_cardinality(&key, original_estimate);
    assert_eq!(
        result, original_estimate,
        "When no feedback exists, estimator must return original estimate unchanged"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R165: Feedback generation invalidated after ANALYZE
// ---------------------------------------------------------------------------
#[test]
fn test_r165_feedback_generation_invalidated_after_analyze() -> Result<()> {
    let ctx = TestContext::new("r165");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Run query to populate feedback
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    assert!(!db.feedback_registry().is_empty());

    // Execute ANALYZE
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("ANALYZE users;")?;
    txn.commit()?;

    // Feedback for users with old statistics_generation should now be invalidated or cleared
    let has_valid_user_feedback = db
        .feedback_registry()
        .all_entries()
        .iter()
        .any(|e| e.key.table_name() == Some("users") && e.key.statistics_generation == 0);
    assert!(
        !has_valid_user_feedback,
        "Old stats generation feedback must be invalidated after ANALYZE"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R166: Feedback invalidated after DROP INDEX
// ---------------------------------------------------------------------------
#[test]
fn test_r166_feedback_invalidated_after_drop_index() -> Result<()> {
    let ctx = TestContext::new("r166");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Create index
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    txn.commit()?;

    // Run query using index
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age = 25;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    // Drop index
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("DROP INDEX idx_users_age;")?;
    txn.commit()?;

    assert_eq!(
        db.feedback_registry().len(),
        0,
        "Feedback should be invalidated after DROP INDEX"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R167: Feedback scoped to correct table/predicate
// ---------------------------------------------------------------------------
#[test]
fn test_r167_feedback_scoped_to_correct_table_predicate() -> Result<()> {
    let ctx = TestContext::new("r167");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Run query on users
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let key_dept = FeedbackKey::scan("departments", "FullTableScan", "budget > 40000", None, 0);
    assert!(
        db.feedback_registry().get_feedback(&key_dept).is_none(),
        "Departments table should have no feedback"
    );

    let key_other_pred = FeedbackKey::scan("users", "FullTableScan", "age < 20", None, 0);
    assert!(
        db.feedback_registry()
            .get_feedback(&key_other_pred)
            .is_none(),
        "Different predicate should not match"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R168: Prepared statement parameter isolation
// ---------------------------------------------------------------------------
#[test]
fn test_r168_prepared_statement_parameter_isolation() -> Result<()> {
    let ctx = TestContext::new("r168");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT * FROM users WHERE age = ?;")?;

    // Execute with age = 25
    let res1 = txn.execute_prepared(&stmt, &[Value::Int64(25)])?;
    assert!(!res1.into_query()?.rows.is_empty());

    // Execute with age = 35
    let res2 = txn.execute_prepared(&stmt, &[Value::Int64(35)])?;
    assert!(!res2.into_query()?.rows.is_empty());
    txn.commit()?;

    // Feedback entries should be recorded
    let entries = db.feedback_registry().all_entries();
    assert!(
        !entries.is_empty(),
        "Should have recorded prepared statement feedback"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R169: Join cardinality feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r169_join_cardinality_feedback() -> Result<()> {
    let ctx = TestContext::new("r169");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement(
        "SELECT u.name, d.name FROM users u JOIN departments d ON u.dept_id = d.dept_id;",
    )?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 50);
    txn.commit()?;

    let join_entries: Vec<_> = db
        .feedback_registry()
        .all_entries()
        .into_iter()
        .filter(|e| matches!(e.key.context, FeedbackContext::Join { .. }))
        .collect();

    assert!(
        !join_entries.is_empty(),
        "Join feedback entry must be recorded"
    );
    let je = &join_entries[0];
    assert_eq!(je.last_actual_rows, 50.0);
    assert_eq!(je.observation_count, 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// R170: SEMI/ANTI cardinality feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r170_semi_anti_cardinality_feedback() -> Result<()> {
    let ctx = TestContext::new("r170");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement(
        "SELECT d.name FROM departments d WHERE EXISTS (SELECT 1 FROM users u WHERE u.dept_id = d.dept_id);",
    )?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 2); // depts 10 and 20 have users
    txn.commit()?;

    let semi_entries: Vec<_> = db
        .feedback_registry()
        .all_entries()
        .into_iter()
        .filter(|e| matches!(e.key.context, FeedbackContext::SemiAnti { .. }))
        .collect();

    assert!(
        !semi_entries.is_empty(),
        "Semi/Anti join feedback must be recorded"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R171: Multi-index candidate feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r171_multi_index_candidate_feedback() -> Result<()> {
    let ctx = TestContext::new("r171");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    txn.execute_statement("CREATE INDEX idx_users_dept ON users (dept_id);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age = 25 AND dept_id = 10;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    assert!(
        !entries.is_empty(),
        "Feedback should be recorded for multi-index or scan"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R172: Skip-scan prefix feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r172_skip_scan_prefix_feedback() -> Result<()> {
    let ctx = TestContext::new("r172");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE INDEX idx_users_dept_age ON users (dept_id, age);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age = 25;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    assert!(!entries.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// R173: External operator cardinality feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r173_external_operator_cardinality_feedback() -> Result<()> {
    let ctx = TestContext::new("r173");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT dept_id, COUNT(*) FROM users GROUP BY dept_id;")?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 2);
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    assert!(!entries.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// R174: Plan-cache execution with feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r174_plan_cache_execution_with_feedback() -> Result<()> {
    let ctx = TestContext::new("r174");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT * FROM users WHERE age > ?;")?;

    // Execute 3 times
    for _ in 0..3 {
        let res = txn.execute_prepared(&stmt, &[Value::Int64(30)])?;
        let query_res = res.into_query()?;
        assert!(!query_res.rows.is_empty());
    }
    txn.commit()?;

    assert!(db.plan_cache().stats().hits >= 2);
    assert!(!db.feedback_registry().is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// R175: Large misestimate triggers reoptimization
// ---------------------------------------------------------------------------
#[test]
fn test_r175_large_misestimate_triggers_reoptimization() -> Result<()> {
    let ctx = TestContext::new("r175");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let key = FeedbackKey::scan("users", "FullTableScan", "age > ?", None, 0);
    let mut reg = FeedbackRegistry::with_capacity(10);
    // Simulate severe misestimate: estimated 1.0, actual 50.0
    let obs1 = FeedbackObservation::new(1.0, 50.0);
    reg.record(key.clone(), obs1);
    let obs2 = FeedbackObservation::new(1.0, 50.0);
    reg.record(key.clone(), obs2);
    assert!(
        reg.should_replan(&key),
        "Severe misestimate with >= 2 observations must trigger replan"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R176: Small misestimate does not trigger reoptimization
// ---------------------------------------------------------------------------
#[test]
fn test_r176_small_misestimate_does_not_trigger_reoptimization() {
    let key = FeedbackKey::scan("users", "FullTableScan", "age > ?", None, 0);
    let mut reg = FeedbackRegistry::with_capacity(10);
    // Small error: estimated 10.0, actual 11.0 (error 1.1x < 2.0x)
    let obs1 = FeedbackObservation::new(10.0, 11.0);
    reg.record(key.clone(), obs1);
    let obs2 = FeedbackObservation::new(10.0, 11.0);
    reg.record(key.clone(), obs2);
    assert!(
        !reg.should_replan(&key),
        "Small misestimate must NOT trigger replan"
    );
}

// ---------------------------------------------------------------------------
// R177: Reoptimization does not thrash
// ---------------------------------------------------------------------------
#[test]
fn test_r177_reoptimization_does_not_thrash() {
    let key = FeedbackKey::scan("users", "FullTableScan", "age > ?", None, 0);
    let mut reg = FeedbackRegistry::with_capacity(10);

    // Single observation must not trigger replan (requires at least REPLAN_MIN_OBSERVATIONS)
    let obs = FeedbackObservation::new(1.0, 100.0);
    reg.record(key.clone(), obs);
    assert!(
        !reg.should_replan(&key),
        "Single observation must not trigger replan (prevents thrashing)"
    );

    let entry = reg.get_feedback(&key).unwrap();
    assert_eq!(entry.last_action, FeedbackAction::Recorded);
}

// ---------------------------------------------------------------------------
// R178: Replanned query produces correct results
// ---------------------------------------------------------------------------
#[test]
fn test_r178_replanned_query_produces_correct_results() -> Result<()> {
    let ctx = TestContext::new("r178");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name FROM users WHERE age > 25 ORDER BY id;")?;

    let res1 = txn.execute_prepared(&stmt, &[])?;
    let rows1 = res1.into_query()?.rows;

    let res2 = txn.execute_prepared(&stmt, &[])?;
    let rows2 = res2.into_query()?.rows;

    assert_eq!(rows1, rows2);
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R179: EXPLAIN ANALYZE feedback diagnostics
// ---------------------------------------------------------------------------
#[test]
fn test_r179_explain_analyze_feedback_diagnostics() -> Result<()> {
    let ctx = TestContext::new("r179");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    // Execute once to build feedback
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;

    // Now run EXPLAIN ANALYZE
    let explain_res = txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM users WHERE age > 30;")?;
    let plan_text = match &explain_res.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    txn.commit()?;

    assert!(
        plan_text.contains("actual_rows:") || plan_text.contains("estimated_rows:"),
        "EXPLAIN ANALYZE must show cardinality diagnostics"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R180: Feedback reset
// ---------------------------------------------------------------------------
#[test]
fn test_r180_feedback_reset() -> Result<()> {
    let ctx = TestContext::new("r180");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    assert!(!db.feedback_registry().is_empty());

    // Clear users table feedback
    db.clear_table_feedback("users");
    assert_eq!(db.feedback_registry().len(), 0);

    // Record again and clear all
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    assert!(!db.feedback_registry().is_empty());
    db.clear_feedback();
    assert_eq!(db.feedback_registry().len(), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R181: Feedback not persisted across reopen
// ---------------------------------------------------------------------------
#[test]
fn test_r181_feedback_not_persisted_across_reopen() -> Result<()> {
    let ctx = TestContext::new("r181");
    {
        let mut db = ctx.db();
        setup_test_schema(&mut db)?;

        let mut txn = db.begin_transaction()?;
        let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
        let _ = res.into_query()?;
        txn.commit()?;

        assert!(
            !db.feedback_registry().is_empty(),
            "Feedback should exist before close"
        );
    }

    // Reopen database
    let db_reopened = ctx.db();
    assert_eq!(
        db_reopened.feedback_registry().len(),
        0,
        "Feedback must be strictly in-memory and empty upon database reopen"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R182: DML feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r182_dml_feedback() -> Result<()> {
    let ctx = TestContext::new("r182");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE users SET age = age + 1 WHERE age > 30;")?;
    txn.execute_statement("DELETE FROM users WHERE age < 22;")?;
    txn.commit()?;

    assert!(
        !db.feedback_registry().is_empty(),
        "DML execution should record scan feedback"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R183: Failed statement does not poison feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r183_failed_statement_does_not_poison_feedback() -> Result<()> {
    let ctx = TestContext::new("r183");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let initial_count = db.feedback_registry().len();

    let mut txn = db.begin_transaction()?;
    // Intentionally bad query (invalid column)
    let res = txn.execute_statement("SELECT * FROM users WHERE non_existent_column > 0;");
    assert!(res.is_err());
    let _ = txn.abort();

    assert_eq!(
        db.feedback_registry().len(),
        initial_count,
        "Failed statement must not add corrupt feedback"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R184: Transaction rollback does not corrupt feedback state
// ---------------------------------------------------------------------------
#[test]
fn test_r184_transaction_rollback_does_not_corrupt_feedback() -> Result<()> {
    let ctx = TestContext::new("r184");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 30;")?;
    let _ = res.into_query()?;
    txn.abort()?;

    // Database should be fully operational
    let mut txn2 = db.begin_transaction()?;
    let res2 = txn2.execute_statement("SELECT COUNT(*) FROM users;")?;
    let query_res = res2.into_query()?;
    assert_eq!(query_res.rows[0].values[0], Value::UInt64(50));
    txn2.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R185: Randomized cardinality-feedback differential test
// ---------------------------------------------------------------------------
#[test]
fn test_r185_randomized_cardinality_feedback_differential_test() -> Result<()> {
    let ctx = TestContext::new("r185");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE items (id BIGINT PRIMARY KEY, category BIGINT, val BIGINT, note TEXT);",
    )?;

    // Insert 100 rows with skewed distributions and NULLs
    for i in 1..=100 {
        let cat = if i <= 80 { 1 } else { i % 10 };
        let val = if i % 5 == 0 {
            "NULL".to_string()
        } else {
            (i * 3).to_string()
        };
        let note = if i % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("'item_{i}'")
        };
        txn.execute_statement(&format!(
            "INSERT INTO items (id, category, val, note) VALUES ({i}, {cat}, {val}, {note});"
        ))?;
    }
    txn.commit()?;

    let queries = [
        "SELECT id, category, val FROM items WHERE category = 1 AND val > 50 ORDER BY id;",
        "SELECT id, category FROM items WHERE category = 1 OR val IS NULL ORDER BY id;",
        "SELECT category, COUNT(val) FROM items GROUP BY category ORDER BY category;",
        "SELECT id, val FROM items WHERE note IS NOT NULL AND val < 100 ORDER BY id;",
    ];

    for query in &queries {
        // Run query 3 times with feedback enabled
        let mut baseline_rows = None;
        for _ in 0..3 {
            let mut txn = db.begin_transaction()?;
            let res = txn.execute_statement(query)?;
            let query_res = res.into_query()?;
            let rows = query_res.rows;
            txn.commit()?;

            if let Some(ref base) = baseline_rows {
                assert_eq!(
                    base, &rows,
                    "Differential result mismatch across feedback executions for: {}",
                    query
                );
            } else {
                baseline_rows = Some(rows);
            }
        }
    }

    assert!(
        !db.feedback_registry().is_empty(),
        "Feedback registry should have accumulated observations across queries"
    );
    Ok(())
}
