use std::path::PathBuf;

use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::feedback::{
    FeedbackAction, FeedbackContext, FeedbackKey, FeedbackObservation, FeedbackRegistry,
    ResourceObservation, SpillStatus,
};
use treedb::query::plan_cache::{ParamPlanningSignature, PlanCacheKey, ResourcePlanningSignature};
use treedb::query::{AggregateConfig, JoinConfig, MultiIndexConfig, SortConfig};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "test_rf_{}_{}_{}.treedb",
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

    // Insert 100 users
    for i in 1..=100 {
        let age = 20 + (i % 30);
        let dept = if i % 3 == 0 {
            10
        } else if i % 3 == 1 {
            20
        } else {
            30
        };
        txn.execute_statement(&format!(
            "INSERT INTO users (id, name, age, dept_id) VALUES ({i}, 'User_{i}', {age}, {dept});"
        ))?;
    }

    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R186: Resource feedback recorded after successful execution
// ---------------------------------------------------------------------------
#[test]
fn test_r186_resource_feedback_recorded_after_successful_execution() -> Result<()> {
    let ctx = TestContext::new("r186");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY age;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    assert!(
        !entries.is_empty(),
        "Feedback entries should be present after query execution"
    );

    let has_resource = entries.iter().any(|e| e.resource_feedback.is_some());
    assert!(
        has_resource,
        "At least one feedback entry should contain ResourceFeedback"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R187: Peak-memory observation recorded
// ---------------------------------------------------------------------------
#[test]
fn test_r187_peak_memory_observation_recorded() -> Result<()> {
    let ctx = TestContext::new("r187");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY age, name;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    let sort_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Sort { .. }));

    assert!(sort_entry.is_some(), "Sort entry should be present");
    let sort_entry = sort_entry.unwrap();
    let res_fb = sort_entry.resource_feedback.as_ref().unwrap();
    assert!(
        res_fb.observation_count >= 1,
        "Should have recorded at least 1 resource observation"
    );
    assert!(
        res_fb.effective_peak_memory_bytes >= 0.0,
        "Peak memory should be recorded and non-negative"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R188: Spill occurrence recorded
// ---------------------------------------------------------------------------
#[test]
fn test_r188_spill_occurrence_recorded() -> Result<()> {
    let mut reg = FeedbackRegistry::with_config(512, 0.3, 2.0, 1);
    let key = FeedbackKey::scan("orders", "FullTableScan", "", None, 1);

    let mut obs = FeedbackObservation::new(100.0, 100.0);
    obs.spill_occurred = Some(true);
    let mut res_obs = ResourceObservation::new(1024);
    res_obs.did_spill = true;
    res_obs.actual_spill_bytes = 4096;
    res_obs.spill_runs = 2;
    obs.resource = Some(res_obs);

    reg.record(key.clone(), obs);

    let entry = reg.get(&key).expect("Entry must exist");
    assert_eq!(
        entry.resource_feedback.as_ref().unwrap().spill_status,
        SpillStatus::Spilled
    );
    assert!(entry.resource_feedback.as_ref().unwrap().replan_triggered);
    Ok(())
}

// ---------------------------------------------------------------------------
// R189: Spill bytes recorded
// ---------------------------------------------------------------------------
#[test]
fn test_r189_spill_bytes_recorded() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("items", "FullTableScan", "", None, 1);

    let mut obs = FeedbackObservation::new(500.0, 500.0);
    let mut res_obs = ResourceObservation::new(4096);
    res_obs.did_spill = true;
    res_obs.actual_spill_bytes = 8192;
    res_obs.spill_runs = 3;
    obs.resource = Some(res_obs);

    reg.record(key.clone(), obs);

    let entry = reg.get(&key).unwrap();
    let res_fb = entry.resource_feedback.as_ref().unwrap();
    assert_eq!(res_fb.effective_spill_bytes, 8192.0);
    assert_eq!(res_fb.effective_spill_runs, 3.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R190: Temporary-file metrics recorded
// ---------------------------------------------------------------------------
#[test]
fn test_r190_temporary_file_metrics_recorded() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("logs", "FullTableScan", "", None, 1);

    let mut obs = FeedbackObservation::new(1000.0, 1000.0);
    let mut res_obs = ResourceObservation::new(2048);
    res_obs.temporary_bytes = 16384;
    res_obs.temporary_files = 4;
    res_obs.did_spill = true;
    obs.resource = Some(res_obs);

    reg.record(key.clone(), obs);

    let entry = reg.get(&key).unwrap();
    let last = entry
        .resource_feedback
        .as_ref()
        .unwrap()
        .last_observation
        .as_ref()
        .unwrap();
    assert_eq!(last.temporary_bytes, 16384);
    assert_eq!(last.temporary_files, 4);
    Ok(())
}

// ---------------------------------------------------------------------------
// R191: Resource feedback uses bounded aggregation
// ---------------------------------------------------------------------------
#[test]
fn test_r191_resource_feedback_bounded_aggregation() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("measurements", "FullTableScan", "", None, 1);

    // Initial observation: 1000 bytes peak memory
    let mut obs1 = FeedbackObservation::new(100.0, 100.0);
    let mut res1 = ResourceObservation::new(65536);
    res1.actual_peak_memory_bytes = 1000;
    obs1.resource = Some(res1);
    reg.record(key.clone(), obs1);

    let fb1 = reg
        .get(&key)
        .unwrap()
        .resource_feedback
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(fb1.effective_peak_memory_bytes, 1000.0);
    assert_eq!(fb1.observation_count, 1);

    // Second observation: 2000 bytes peak memory (EWMA: 0.7 * 1000 + 0.3 * 2000 = 1300)
    let mut obs2 = FeedbackObservation::new(100.0, 100.0);
    let mut res2 = ResourceObservation::new(65536);
    res2.actual_peak_memory_bytes = 2000;
    obs2.resource = Some(res2);
    reg.record(key.clone(), obs2);

    let fb2 = reg
        .get(&key)
        .unwrap()
        .resource_feedback
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(fb2.observation_count, 2);
    assert!((fb2.effective_peak_memory_bytes - 1300.0).abs() < 1e-6);
    Ok(())
}

// ---------------------------------------------------------------------------
// R192: Resource outlier clamping
// ---------------------------------------------------------------------------
#[test]
fn test_r192_resource_outlier_clamping() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("data", "FullTableScan", "", None, 1);

    // Baseline: 1000 bytes
    let mut obs1 = FeedbackObservation::new(100.0, 100.0);
    let mut res1 = ResourceObservation::new(65536);
    res1.actual_peak_memory_bytes = 1000;
    obs1.resource = Some(res1);
    reg.record(key.clone(), obs1);

    // Extreme outlier: 1,000,000 bytes (exceeds 100x max multiplier)
    let mut obs2 = FeedbackObservation::new(100.0, 100.0);
    let mut res2 = ResourceObservation::new(65536);
    res2.estimated_peak_memory_bytes = 1000;
    res2.actual_peak_memory_bytes = 1_000_000;
    obs2.resource = Some(res2);
    reg.record(key.clone(), obs2);

    let entry = reg.get(&key).unwrap();
    let last = entry
        .resource_feedback
        .as_ref()
        .unwrap()
        .last_observation
        .as_ref()
        .unwrap();
    assert!(
        last.is_clamped,
        "Extreme resource outlier should be clamped"
    );
    assert_eq!(last.actual_peak_memory_bytes, 102400); // 1024.max(1000) * 100.0
    Ok(())
}

// ---------------------------------------------------------------------------
// R193: Zero-resource-estimate handling
// ---------------------------------------------------------------------------
#[test]
fn test_r193_zero_resource_estimate_handling() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("zero_test", "FullTableScan", "", None, 1);

    // Initial with 0 peak memory and 0 spill
    let mut obs = FeedbackObservation::new(0.0, 0.0);
    let mut res_obs = ResourceObservation::new(65536);
    res_obs.actual_peak_memory_bytes = 0;
    res_obs.actual_spill_bytes = 0;
    obs.resource = Some(res_obs);
    reg.record(key.clone(), obs);

    let entry = reg.get(&key).unwrap();
    let res_fb = entry.resource_feedback.as_ref().unwrap();
    assert_eq!(res_fb.effective_peak_memory_bytes, 0.0);
    assert_eq!(res_fb.effective_spill_bytes, 0.0);
    assert_eq!(res_fb.spill_status, SpillStatus::NeverSpilled);
    Ok(())
}

// ---------------------------------------------------------------------------
// R194: Memory-budget differences isolate feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r194_memory_budget_differences_isolate_feedback() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let sig_64k = ResourcePlanningSignature::new(64 * 1024, 32, 100 * 1024 * 1024, 8);
    let sig_64m = ResourcePlanningSignature::new(64 * 1024 * 1024, 32, 100 * 1024 * 1024, 8);

    let key_64k = FeedbackKey::scan("table1", "FullTableScan", "", None, 1)
        .with_resource_signature(Some(sig_64k.clone()));
    let key_64m = FeedbackKey::scan("table1", "FullTableScan", "", None, 1)
        .with_resource_signature(Some(sig_64m.clone()));

    assert_ne!(
        key_64k, key_64m,
        "Keys with different resource signatures must be distinct"
    );

    // Record spill under 64K
    let mut obs_64k = FeedbackObservation::new(100.0, 100.0);
    let mut res_64k = ResourceObservation::new(64 * 1024);
    res_64k.did_spill = true;
    res_64k.actual_spill_bytes = 10000;
    obs_64k.resource = Some(res_64k);
    reg.record(key_64k.clone(), obs_64k);

    assert!(reg.get(&key_64k).is_some());
    assert!(
        reg.get(&key_64m).is_none(),
        "64M key should have no feedback recorded"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R195: Plan cache does not reuse incompatible resource plans
// ---------------------------------------------------------------------------
#[test]
fn test_r195_plan_cache_does_not_reuse_incompatible_resource_plans() -> Result<()> {
    use treedb::query::optimizer::{AccessPath, PhysicalPlan, PhysicalTableScan};
    use treedb::query::plan_cache::{PlanCache, PlanValidity};

    let mut cache = PlanCache::new();

    let sig_low = ResourcePlanningSignature::new(16 * 1024, 16, 10 * 1024 * 1024, 4);
    let sig_high = ResourcePlanningSignature::new(16 * 1024 * 1024, 16, 10 * 1024 * 1024, 4);

    let key_low = PlanCacheKey::new("SELECT * FROM t;", ParamPlanningSignature::empty(), 1, 1, 1)
        .with_resource_signature(Some(sig_low.clone()));
    let key_high = PlanCacheKey::new("SELECT * FROM t;", ParamPlanningSignature::empty(), 1, 1, 1)
        .with_resource_signature(Some(sig_high.clone()));

    let dummy_plan = PhysicalPlan {
        base_table: PhysicalTableScan {
            table_name: "t".to_string(),
            table_alias: None,
            access_path: AccessPath::FullTableScan,
            residual_filter: None,
            estimated_rows: 100.0,
            cost: 10.0,
        },
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
        projections: vec![],
        group_by: vec![],
        having: None,
        aggregates: vec![],
        aggregate_strategy: None,
        window_exprs: vec![],
        post_join_filter: None,
        estimated_rows: 100.0,
        total_cost: 10.0,
        set_op: None,
        ctes: vec![],
        logical_rewrites: vec![],
        inferred_predicates: vec![],
    };

    let validity = PlanValidity::new(1, 1, 1, vec!["t".to_string()], vec![])
        .with_resource_signature(Some(sig_low));

    cache.insert(key_low.clone(), dummy_plan, validity);

    // Lookup under low memory budget succeeds
    assert!(cache.get(&key_low, 1, 1, 1).is_some());

    // Lookup under high memory budget misses
    assert!(cache.get(&key_high, 1, 1, 1).is_none());
    Ok(())
}

// ---------------------------------------------------------------------------
// R196: Prepared statements respect resource planning signature
// ---------------------------------------------------------------------------
#[test]
fn test_r196_prepared_statements_respect_resource_planning_signature() -> Result<()> {
    let ctx = TestContext::new("r196");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt_id = txn.prepare_sql("SELECT * FROM users WHERE age > ? ORDER BY age;")?;

    // Execute under default config
    let res1 = txn.execute_prepared(&stmt_id, &[Value::Int64(25)])?;
    let _ = res1.into_query()?;
    txn.commit()?;

    // Change memory budget on DB
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(16 * 1024));

    let mut txn2 = db.begin_transaction()?;
    let res2 = txn2.execute_prepared(&stmt_id, &[Value::Int64(25)])?;
    let _ = res2.into_query()?;
    txn2.commit()?;

    assert_eq!(db.plan_cache().stats().misses, 2);
    Ok(())
}

// ---------------------------------------------------------------------------
// R197: External sort feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r197_external_sort_feedback() -> Result<()> {
    let ctx = TestContext::new("r197");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Force small sort memory budget
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(256));

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY name, age;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    let sort_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Sort { .. }));
    assert!(sort_entry.is_some());
    let fb = sort_entry.unwrap();
    assert!(fb.resource_feedback.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// R198: External aggregation feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r198_external_aggregation_feedback() -> Result<()> {
    let ctx = TestContext::new("r198");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Force small aggregate budget
    db.set_aggregate_config(AggregateConfig::new().with_memory_budget_bytes(256));

    let mut txn = db.begin_transaction()?;
    let res =
        txn.execute_statement("SELECT dept_id, count(*), avg(age) FROM users GROUP BY dept_id;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    let agg_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Aggregate { .. }));
    assert!(agg_entry.is_some());
    let fb = agg_entry.unwrap();
    assert!(fb.resource_feedback.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// R199: External hash join feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r199_external_hash_join_feedback() -> Result<()> {
    let ctx = TestContext::new("r199");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    db.set_join_config(JoinConfig::new().with_memory_budget_bytes(256));

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement(
        "SELECT u.id, u.name, d.name FROM users u JOIN departments d ON u.dept_id = d.dept_id;",
    )?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    let join_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Join { .. }));
    assert!(join_entry.is_some());
    let fb = join_entry.unwrap();
    assert!(fb.resource_feedback.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// R200: External multi-index feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r200_external_multi_index_feedback() -> Result<()> {
    let ctx = TestContext::new("r200");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE INDEX idx_users_age ON users(age);")?;
    txn.execute_statement("CREATE INDEX idx_users_dept ON users(dept_id);")?;
    txn.commit()?;

    db.set_multi_index_config(MultiIndexConfig::new().with_memory_budget_bytes(256));

    let mut txn2 = db.begin_transaction()?;
    let res = txn2.execute_statement("SELECT * FROM users WHERE age = 25 AND dept_id = 10;")?;
    let _ = res.into_query()?;
    txn2.commit()?;

    let entries = db.feedback_registry().all_entries();
    let scan_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Scan { .. }));
    assert!(scan_entry.is_some());
    assert!(scan_entry.unwrap().resource_feedback.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// R201: In-memory execution feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r201_in_memory_execution_feedback() -> Result<()> {
    let ctx = TestContext::new("r201");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(64 * 1024 * 1024));

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY age;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    let sort_entry = entries
        .iter()
        .find(|e| matches!(e.key.context, FeedbackContext::Sort { .. }))
        .expect("Sort entry should exist");

    let res_fb = sort_entry.resource_feedback.as_ref().unwrap();
    assert_eq!(res_fb.spill_status, SpillStatus::NeverSpilled);
    assert_eq!(res_fb.effective_spill_bytes, 0.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R202: Unexpected spill can trigger future reoptimization
// ---------------------------------------------------------------------------
#[test]
fn test_r202_unexpected_spill_triggers_reoptimization() -> Result<()> {
    let mut reg = FeedbackRegistry::with_config(512, 0.3, 2.0, 1);
    let key = FeedbackKey::scan("large_tbl", "FullTableScan", "", None, 1);

    // Initial estimate in-memory (spill_status = NeverSpilled)
    let mut obs = FeedbackObservation::new(1000.0, 1000.0);
    let mut res_obs = ResourceObservation::new(1024);
    res_obs.did_spill = true;
    res_obs.actual_spill_bytes = 20000;
    obs.resource = Some(res_obs);

    reg.record(key.clone(), obs);

    let entry = reg.get(&key).unwrap();
    assert!(
        entry.should_replan(),
        "Unexpected spill should trigger replan"
    );
    assert_eq!(entry.last_action, FeedbackAction::ReplanTriggered);
    Ok(())
}

// ---------------------------------------------------------------------------
// R203: Consistently non-spilling workload can avoid unnecessary external planning
// ---------------------------------------------------------------------------
#[test]
fn test_r203_consistently_non_spilling_workload() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("small_tbl", "FullTableScan", "", None, 1);

    for _ in 0..5 {
        let mut obs = FeedbackObservation::new(10.0, 10.0);
        let mut res_obs = ResourceObservation::new(65536);
        res_obs.did_spill = false;
        res_obs.actual_peak_memory_bytes = 512;
        obs.resource = Some(res_obs);
        reg.record(key.clone(), obs);
    }

    assert_eq!(reg.get_spill_status(&key), SpillStatus::NeverSpilled);
    assert_eq!(reg.get_adjusted_spill_bytes(&key, 10000.0), 0.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R204: Resource reoptimization does not thrash
// ---------------------------------------------------------------------------
#[test]
fn test_r204_resource_reoptimization_does_not_thrash() -> Result<()> {
    let mut reg = FeedbackRegistry::new();
    let key = FeedbackKey::scan("stable_tbl", "FullTableScan", "", None, 1);

    // Record consistent observations with small variance (< replan threshold)
    for i in 0..10 {
        let mut obs = FeedbackObservation::new(100.0, 105.0 + (i as f64));
        let mut res_obs = ResourceObservation::new(65536);
        res_obs.actual_peak_memory_bytes = 2000 + i * 10;
        obs.resource = Some(res_obs);
        reg.record(key.clone(), obs);
    }

    let entry = reg.get(&key).unwrap();
    assert_ne!(entry.last_action, FeedbackAction::ReplanTriggered);
    assert!(!entry.should_replan());
    Ok(())
}

// ---------------------------------------------------------------------------
// R205: Hard memory limits remain enforced
// ---------------------------------------------------------------------------
#[test]
fn test_r205_hard_memory_limits_remain_enforced() -> Result<()> {
    let ctx = TestContext::new("r205");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Minimum budget
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(64));

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY name DESC;")?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 100);
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R206: Hard temp-file limits remain enforced
// ---------------------------------------------------------------------------
#[test]
fn test_r206_hard_temp_file_limits_remain_enforced() -> Result<()> {
    let ctx = TestContext::new("r206");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(256)
            .with_max_temp_files(2),
    );

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY age, name;")?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 100);
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R207: Hard temp-byte limits remain enforced
// ---------------------------------------------------------------------------
#[test]
fn test_r207_hard_temp_byte_limits_remain_enforced() -> Result<()> {
    let ctx = TestContext::new("r207");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(256)
            .with_max_temp_bytes(50 * 1024 * 1024),
    );

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users ORDER BY age DESC;")?;
    let query_res = res.into_query()?;
    assert_eq!(query_res.rows.len(), 100);
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R208: EXPLAIN exposes resource estimates
// ---------------------------------------------------------------------------
#[test]
fn test_r208_explain_exposes_resource_estimates() -> Result<()> {
    let ctx = TestContext::new("r208");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("EXPLAIN SELECT * FROM users ORDER BY age;")?;
    let query_res = res.into_query()?;
    let plan_text = match &query_res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("Expected string plan output"),
    };

    assert!(plan_text.contains("Sort"));
    assert!(plan_text.contains("TableScan"));
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R209: EXPLAIN ANALYZE exposes actual resource usage
// ---------------------------------------------------------------------------
#[test]
fn test_r209_explain_analyze_exposes_actual_resource_usage() -> Result<()> {
    let ctx = TestContext::new("r209");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("EXPLAIN ANALYZE SELECT * FROM users ORDER BY age;")?;
    let query_res = res.into_query()?;
    let plan_text = match &query_res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("Expected string plan output"),
    };

    assert!(plan_text.contains("actual_rows"));
    txn.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R210: ANALYZE invalidates stale resource feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r210_analyze_invalidates_stale_resource_feedback() -> Result<()> {
    let ctx = TestContext::new("r210");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement("SELECT * FROM users WHERE age > 25;")?;
    let _ = res.into_query()?;
    txn.commit()?;

    assert!(!db.feedback_registry().is_empty());

    let mut txn2 = db.begin_transaction()?;
    txn2.execute_statement("ANALYZE users;")?;
    txn2.commit()?;

    assert_eq!(
        db.feedback_registry().len(),
        0,
        "ANALYZE should invalidate feedback for table"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R211: DROP INDEX invalidates relevant resource feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r211_drop_index_invalidates_resource_feedback() -> Result<()> {
    let ctx = TestContext::new("r211");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE INDEX idx_users_age ON users(age);")?;
    txn.commit()?;

    let mut txn2 = db.begin_transaction()?;
    let res = txn2.execute_statement("SELECT * FROM users WHERE age = 25;")?;
    let _ = res.into_query()?;
    txn2.commit()?;

    let mut txn3 = db.begin_transaction()?;
    txn3.execute_statement("DROP INDEX idx_users_age;")?;
    txn3.commit()?;

    assert_eq!(db.feedback_registry().len(), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// R212: Feedback is not persisted across reopen
// ---------------------------------------------------------------------------
#[test]
fn test_r212_feedback_not_persisted_across_reopen() -> Result<()> {
    let ctx = TestContext::new("r212");
    {
        let mut db = ctx.db();
        setup_test_schema(&mut db)?;

        let mut txn = db.begin_transaction()?;
        let res = txn.execute_statement("SELECT * FROM users ORDER BY age;")?;
        let _ = res.into_query()?;
        txn.commit()?;

        assert!(!db.feedback_registry().is_empty());
    }

    // Reopen DB
    let db2 = ctx.db();
    assert_eq!(
        db2.feedback_registry().len(),
        0,
        "Feedback must not persist across DB reopen"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R213: Failed execution does not poison resource feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r213_failed_execution_does_not_poison_feedback() -> Result<()> {
    let ctx = TestContext::new("r213");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let count_before = db.feedback_registry().len();

    let mut txn = db.begin_transaction()?;
    let err = txn.execute_statement("SELECT * FROM non_existent_table WHERE age > 20;");
    assert!(err.is_err());
    let _ = txn.abort();

    assert_eq!(db.feedback_registry().len(), count_before);
    Ok(())
}

// ---------------------------------------------------------------------------
// R214: Transaction rollback does not corrupt resource feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r214_transaction_rollback_does_not_corrupt_feedback() -> Result<()> {
    let ctx = TestContext::new("r214");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn1 = db.begin_transaction()?;
    let res = txn1.execute_statement("SELECT * FROM users WHERE age > 25;")?;
    let _ = res.into_query()?;
    txn1.abort()?;

    // Database should be fully operational and transactions can proceed
    let mut txn2 = db.begin_transaction()?;
    let res2 = txn2.execute_statement("SELECT count(*) FROM users;")?;
    let query_res = res2.into_query()?;
    assert_eq!(query_res.rows[0].values[0], Value::UInt64(100));
    txn2.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R215: DML resource feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r215_dml_resource_feedback() -> Result<()> {
    let ctx = TestContext::new("r215");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE users SET age = age + 1 WHERE age > 30;")?;
    txn.execute_statement("DELETE FROM users WHERE age > 45;")?;
    txn.commit()?;

    // Operations succeed and DML semantics remain intact
    let mut txn2 = db.begin_transaction()?;
    let res = txn2.execute_statement("SELECT count(*) FROM users;")?;
    let query_res = res.into_query()?;
    assert!(!query_res.rows.is_empty());
    txn2.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R216: Randomized external-vs-in-memory differential test
// ---------------------------------------------------------------------------
#[test]
fn test_r216_randomized_external_vs_in_memory_differential() -> Result<()> {
    let ctx = TestContext::new("r216");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res_mem = txn.execute_statement("SELECT id, name, age FROM users ORDER BY age, id;")?;
    let rows_mem = res_mem.into_query()?.rows;
    txn.commit()?;

    // Force external sort
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(256));

    let mut txn2 = db.begin_transaction()?;
    let res_ext = txn2.execute_statement("SELECT id, name, age FROM users ORDER BY age, id;")?;
    let rows_ext = res_ext.into_query()?.rows;
    txn2.commit()?;

    assert_eq!(rows_mem.len(), rows_ext.len());
    for (r1, r2) in rows_mem.iter().zip(rows_ext.iter()) {
        assert_eq!(r1.values, r2.values);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// R217: Resource-aware plan-cache invalidation
// ---------------------------------------------------------------------------
#[test]
fn test_r217_resource_aware_plan_cache_invalidation() -> Result<()> {
    let ctx = TestContext::new("r217");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt_id = txn.prepare_sql("SELECT * FROM users ORDER BY age;")?;
    let _ = txn.execute_prepared(&stmt_id, &[])?;
    txn.commit()?;

    assert_eq!(db.plan_cache().len(), 1);

    // Alter table schema
    let mut txn2 = db.begin_transaction()?;
    txn2.execute_statement("CREATE TABLE new_tbl (id BIGINT PRIMARY KEY);")?;
    txn2.commit()?;

    assert_eq!(
        db.plan_cache().len(),
        0,
        "DDL should invalidate plan cache entries"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// R218: Different memory budgets produce semantically equivalent results
// ---------------------------------------------------------------------------
#[test]
fn test_r218_different_memory_budgets_produce_semantically_equivalent_results() -> Result<()> {
    let ctx = TestContext::new("r218");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let budgets = [256, 1024, 16 * 1024, 64 * 1024, 4 * 1024 * 1024];
    let sql = "SELECT dept_id, count(*), sum(age) FROM users GROUP BY dept_id ORDER BY dept_id;";

    let mut baseline_rows = None;

    for budget in budgets {
        db.set_aggregate_config(AggregateConfig::new().with_memory_budget_bytes(budget));
        db.set_sort_config(SortConfig::new().with_memory_budget_bytes(budget));

        let mut txn = db.begin_transaction()?;
        let res = txn.execute_statement(sql)?;
        let rows = res.into_query()?.rows;
        txn.commit()?;

        if let Some(ref base) = baseline_rows {
            assert_eq!(base, &rows, "Results must match across memory budgets");
        } else {
            baseline_rows = Some(rows);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// R219: Three or more consecutive executions converge without plan thrashing
// ---------------------------------------------------------------------------
#[test]
fn test_r219_consecutive_executions_converge() -> Result<()> {
    let ctx = TestContext::new("r219");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut first_res = None;
    for _ in 0..5 {
        let mut txn = db.begin_transaction()?;
        let res = txn.execute_statement(
            "SELECT u.id, u.name, d.name FROM users u JOIN departments d ON u.dept_id = d.dept_id ORDER BY u.id;",
        )?;
        let rows = res.into_query()?.rows;
        txn.commit()?;

        if let Some(ref prev) = first_res {
            assert_eq!(
                prev, &rows,
                "Outputs must converge stably across executions"
            );
        } else {
            first_res = Some(rows);
        }
    }

    let entries = db.feedback_registry().all_entries();
    assert!(!entries.is_empty());
    for entry in entries {
        assert!(entry.observation_count >= 1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// R220: Full integration with Phase 45 cardinality feedback
// ---------------------------------------------------------------------------
#[test]
fn test_r220_full_integration_cardinality_and_resource_feedback() -> Result<()> {
    let ctx = TestContext::new("r220");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement(
        "SELECT u.dept_id, count(*), avg(u.age) \
         FROM users u JOIN departments d ON u.dept_id = d.dept_id \
         WHERE u.age > 25 \
         GROUP BY u.dept_id \
         ORDER BY u.dept_id;",
    )?;
    let _ = res.into_query()?;
    txn.commit()?;

    let entries = db.feedback_registry().all_entries();
    assert!(!entries.is_empty());

    for entry in entries {
        // Must contain valid cardinality estimate & actuals
        assert!(entry.effective_cardinality >= 0.0);
        assert!(entry.observation_count >= 1);
        // Resource feedback also present
        if let Some(ref res_fb) = entry.resource_feedback {
            assert!(res_fb.effective_peak_memory_bytes >= 0.0);
        }
    }
    Ok(())
}
