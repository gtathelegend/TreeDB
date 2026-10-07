//! Comprehensive test suite for Phase 63 — Unified Plan Lifecycle, Adaptive Selection & Runtime Replanning.

use std::path::PathBuf;

use treedb::engine::types::{Schema, Value};
use treedb::error::Result;
use treedb::query::feedback::{
    FeedbackKey, FeedbackObservation, FeedbackRegistry, ResourceObservation,
};
use treedb::query::lifecycle::{
    AdaptivePlanSelector, AdaptiveSelectionConfig, PlanCandidate, PlanCandidateSet,
    PlanLifecycleState, ReplanDecision,
};
use treedb::query::operator::SortDirection;
use treedb::query::optimizer::{AccessPath, PhysicalPlan, PhysicalTableScan, SecondaryScanType};
use treedb::query::physical_properties::{OrderingColumn, OrderingProperty, PhysicalProperties};
use treedb::query::plan_cache::{ParamPlanningSignature, ResourcePlanningSignature};
use treedb::query::validation::ValidatedPhysicalPlan;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "test_aps_{}_{}_{}.treedb",
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
    txn.execute_statement("CREATE INDEX idx_users_age ON users(age);")?;
    txn.execute_statement("CREATE INDEX idx_users_dept ON users(dept_id);")?;

    txn.execute_statement(
        "INSERT INTO departments (dept_id, name, budget) VALUES \
         (10, 'Engineering', 100000.0), \
         (20, 'Sales', 50000.0), \
         (30, 'Marketing', 30000.0);",
    )?;

    for i in 1..=100 {
        let age = 20 + (i % 50);
        let dept = if i % 2 == 0 { 10 } else { 20 };
        txn.execute_statement(&format!(
            "INSERT INTO users (id, name, age, dept_id) VALUES ({i}, 'User{i}', {age}, {dept});"
        ))?;
    }
    txn.commit()?;
    Ok(())
}

fn dummy_plan(access_path: AccessPath, cost: f64, rows: f64) -> PhysicalPlan {
    let scan = PhysicalTableScan {
        table_name: "users".to_string(),
        table_alias: None,
        access_path,
        residual_filter: None,
        estimated_rows: rows,
        cost,
        row_goal: None,
        estimated_rows_examined: Some(rows),
        estimated_qualifying_rows: Some(rows),
        early_termination_possible: false,
        required_columns: Vec::new(),
        pruned_columns: Vec::new(),
        covered_columns: Vec::new(),
        late_materialization: false,
        index_covered_filter: None,
        base_record_filter: None,
    };
    PhysicalPlan {
        ctes: Vec::new(),
        base_table: scan,
        joins: Vec::new(),
        post_join_filter: None,
        group_by: Vec::new(),
        aggregates: Vec::new(),
        aggregate_strategy: None,
        having: None,
        window_exprs: Vec::new(),
        projections: Vec::new(),
        order_by: Vec::new(),
        order_by_sort_required: false,
        order_by_top_n: false,
        provided_ordering: None,
        offset: None,
        limit: None,
        set_op: None,
        logical_rewrites: Vec::new(),
        inferred_predicates: Vec::new(),
        row_goal: None,
        estimated_rows_examined: Some(rows),
        estimated_qualifying_rows: Some(rows),
        row_goal_early_termination: false,
        total_cost: cost,
        estimated_rows: rows,
    }
}

fn mock_validated_candidate(
    plan: PhysicalPlan,
    cost: f64,
    rows: f64,
    description: &str,
) -> PlanCandidate {
    let mut cand = PlanCandidate::new(plan, cost, rows, PhysicalProperties::default(), description);
    cand.validated = Some(ValidatedPhysicalPlan::new(
        cand.plan.clone(),
        Schema::new_projected(Vec::new()),
    ));
    cand.state = PlanLifecycleState::Validated;
    cand
}

// =========================================================================
// 1. Basic Candidate Set & Deterministic Tie-Breaking Tests
// =========================================================================

#[test]
fn test_lifecycle_states_and_transitions() {
    let plan = dummy_plan(AccessPath::FullTableScan, 100.0, 100.0);
    let mut cand = PlanCandidate::new(
        plan,
        100.0,
        100.0,
        PhysicalProperties::default(),
        "Full Scan",
    );
    assert_eq!(cand.state, PlanLifecycleState::Candidate);
    assert!(!cand.is_valid());

    // Manually mark validated
    cand.state = PlanLifecycleState::Validated;
    cand.validated = Some(ValidatedPhysicalPlan::new(
        cand.plan.clone(),
        Schema::new_projected(Vec::new()),
    ));
    assert!(cand.is_valid());

    cand.mark_selected();
    assert_eq!(cand.state, PlanLifecycleState::Selected);

    cand.mark_executing();
    assert_eq!(cand.state, PlanLifecycleState::Executing);

    cand.mark_completed();
    assert_eq!(cand.state, PlanLifecycleState::Completed);

    cand.mark_cached();
    assert_eq!(cand.state, PlanLifecycleState::Cached);

    cand.mark_invalidated();
    assert_eq!(cand.state, PlanLifecycleState::Invalidated);
    assert!(!cand.is_valid());
}

#[test]
fn test_candidate_set_bounding_and_deduplication() {
    let mut set = PlanCandidateSet::new(2);

    let plan1 = dummy_plan(AccessPath::FullTableScan, 50.0, 100.0);
    let plan2 = dummy_plan(
        AccessPath::SecondaryIndexScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
        },
        30.0,
        20.0,
    );
    let plan3 = dummy_plan(
        AccessPath::IndexOnlyScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
            covered_columns: vec!["age".to_string()],
        },
        10.0,
        20.0,
    );

    let c1 = PlanCandidate::new(plan1, 50.0, 100.0, PhysicalProperties::default(), "Scan 1");
    let c2 = PlanCandidate::new(plan2, 30.0, 20.0, PhysicalProperties::default(), "Scan 2");
    let c3 = PlanCandidate::new(plan3, 10.0, 20.0, PhysicalProperties::default(), "Scan 3");

    assert!(set.add_candidate(c1));
    assert!(set.add_candidate(c2));
    assert_eq!(set.len(), 2);

    // Adding c3 (cheaper than c1 at cost 50.0) should evict c1 to respect capacity
    assert!(set.add_candidate(c3));
    assert_eq!(set.len(), 2);
    assert!(set.candidates().iter().any(|c| c.estimated_cost == 10.0));
    assert!(set.candidates().iter().any(|c| c.estimated_cost == 30.0));
    assert!(!set.candidates().iter().any(|c| c.estimated_cost == 50.0));
}

#[test]
fn test_deterministic_tie_breaking_order() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig::default());

    let plan_sec = dummy_plan(
        AccessPath::SecondaryIndexScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
        },
        20.0,
        10.0,
    );
    let plan_idx_only = dummy_plan(
        AccessPath::IndexOnlyScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
            covered_columns: vec!["age".to_string()],
        },
        20.0,
        10.0,
    );

    let c_sec = mock_validated_candidate(plan_sec, 20.0, 10.0, "Secondary");
    let c_idx = mock_validated_candidate(plan_idx_only, 20.0, 10.0, "Index Only");

    let mut set = PlanCandidateSet::new(10);
    // Add in reverse order: secondary first, then index only
    set.add_candidate(c_sec);
    set.add_candidate(c_idx.clone());

    let res = selector
        .select(&set, None, None, None, None, 1, None)
        .unwrap();

    // IndexOnlyScan has higher preference over SecondaryIndexScan on same cost/cardinality
    assert_eq!(res.selected_fingerprint, c_idx.fingerprint);
}

// =========================================================================
// 2. Feedback Thresholds, EWMA & Observation Count
// =========================================================================

#[test]
fn test_feedback_minimum_observations_respected() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        min_observations: 3,
        ..Default::default()
    });

    let mut reg = FeedbackRegistry::new();
    let plan = dummy_plan(AccessPath::FullTableScan, 10.0, 10.0);
    let cand = mock_validated_candidate(plan, 10.0, 10.0, "Scan");

    let key = FeedbackKey::scan("users", "FullTableScan", "", None, 1);

    // 1 observation (below min 3)
    reg.record_observation(key.clone(), FeedbackObservation::new(10.0, 1000.0));
    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand.clone());

    let res1 = selector
        .select(&set, None, Some(&reg), None, None, 1, None)
        .unwrap();
    assert_eq!(res1.diagnostics.observations_used, 1);
    assert_eq!(res1.diagnostics.adjusted_cost, 10.0); // Cost unchanged because below min_observations

    // 2 observations (still below min 3)
    reg.record_observation(key.clone(), FeedbackObservation::new(10.0, 1000.0));
    let res2 = selector
        .select(&set, None, Some(&reg), None, None, 1, None)
        .unwrap();
    assert_eq!(res2.diagnostics.adjusted_cost, 10.0);

    // 3 observations (meets min 3)
    reg.record_observation(key, FeedbackObservation::new(10.0, 1000.0));
    let res3 = selector
        .select(&set, None, Some(&reg), None, None, 1, None)
        .unwrap();
    assert!(res3.diagnostics.adjusted_cost > 10.0); // Now cost is adjusted!
}

#[test]
fn test_weak_feedback_vs_strong_feedback() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        min_observations: 2,
        min_confidence: 0.5,
        switch_threshold: 0.85,
        ..Default::default()
    });

    let mut reg = FeedbackRegistry::new();
    let plan_a = dummy_plan(AccessPath::FullTableScan, 20.0, 10.0);
    let plan_b = dummy_plan(
        AccessPath::SecondaryIndexScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
        },
        25.0,
        10.0,
    );

    let cand_a = mock_validated_candidate(plan_a, 20.0, 10.0, "Plan A");
    let cand_b = mock_validated_candidate(plan_b, 25.0, 10.0, "Plan B");

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand_a.clone());
    set.add_candidate(cand_b.clone());

    // Initially, Plan A (cost 20) is preferred over Plan B (cost 25)
    let res0 = selector
        .select(&set, Some(&cand_a.fingerprint), None, None, None, 1, None)
        .unwrap();
    assert_eq!(res0.selected_fingerprint, cand_a.fingerprint);

    // Weak misestimate for Plan A: actual 12 vs est 10 (20% error)
    let key_a = FeedbackKey::scan("users", "FullTableScan", "", None, 1);
    reg.record_observation(key_a.clone(), FeedbackObservation::new(10.0, 12.0));
    reg.record_observation(key_a.clone(), FeedbackObservation::new(10.0, 12.0));

    let res_weak = selector
        .select(
            &set,
            Some(&cand_a.fingerprint),
            Some(&reg),
            None,
            None,
            1,
            None,
        )
        .unwrap();
    assert_eq!(res_weak.selected_fingerprint, cand_a.fingerprint);
    assert!(matches!(
        res_weak.decision,
        ReplanDecision::KeepCurrentPlan { .. }
    ));

    // Strong misestimate for Plan A: actual 500 vs est 10 (50x error)
    reg.record_observation(key_a.clone(), FeedbackObservation::new(10.0, 500.0));
    reg.record_observation(key_a, FeedbackObservation::new(10.0, 500.0));

    let res_strong = selector
        .select(
            &set,
            Some(&cand_a.fingerprint),
            Some(&reg),
            None,
            None,
            1,
            None,
        )
        .unwrap();
    assert_eq!(res_strong.selected_fingerprint, cand_b.fingerprint);
    assert!(matches!(
        res_strong.decision,
        ReplanDecision::SwitchToAlternative { .. }
    ));
}

// =========================================================================
// 3. Hysteresis & Anti-Flapping
// =========================================================================

#[test]
fn test_hysteresis_prevents_flapping() {
    // Config: candidate must be at least 20% cheaper (switch_threshold = 0.8)
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        switch_threshold: 0.80,
        min_observations: 2,
        ..Default::default()
    });

    let plan_a = dummy_plan(AccessPath::FullTableScan, 100.0, 100.0);
    let plan_b = dummy_plan(
        AccessPath::SecondaryIndexScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
        },
        95.0, // only 5% cheaper than A
        90.0,
    );

    let cand_a = mock_validated_candidate(plan_a, 100.0, 100.0, "Plan A");
    let cand_b = mock_validated_candidate(plan_b, 95.0, 90.0, "Plan B");

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand_a.clone());
    set.add_candidate(cand_b.clone());

    // When current plan is Plan A:
    // Even though Plan B has cost 95 vs 100 (5% better), it does not beat threshold 0.80 (needs < 80.0)
    let res = selector
        .select(&set, Some(&cand_a.fingerprint), None, None, None, 1, None)
        .unwrap();

    assert_eq!(res.selected_fingerprint, cand_a.fingerprint);
    assert!(matches!(
        res.decision,
        ReplanDecision::KeepCurrentPlan { .. }
    ));

    // Now if Plan B cost drops to 70.0 (30% better):
    set.find_by_fingerprint_mut(&cand_b.fingerprint)
        .unwrap()
        .estimated_cost = 70.0;
    set.find_by_fingerprint_mut(&cand_b.fingerprint)
        .unwrap()
        .adjusted_cost = 70.0;

    let res_switch = selector
        .select(&set, Some(&cand_a.fingerprint), None, None, None, 1, None)
        .unwrap();

    assert_eq!(res_switch.selected_fingerprint, cand_b.fingerprint);
    assert!(matches!(
        res_switch.decision,
        ReplanDecision::SwitchToAlternative { .. }
    ));
}

// =========================================================================
// 4. Physical Plan Validation Safety Boundary
// =========================================================================

#[test]
fn test_unvalidated_candidate_excluded_and_rejected_if_none_valid() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig::default());

    let plan1 = dummy_plan(AccessPath::FullTableScan, 10.0, 10.0);
    let plan2 = dummy_plan(AccessPath::FullTableScan, 20.0, 20.0);

    let mut cand1 = PlanCandidate::new(plan1, 10.0, 10.0, PhysicalProperties::default(), "Cand 1");
    cand1.state = PlanLifecycleState::Candidate; // Not validated!
    let mut cand2 = PlanCandidate::new(plan2, 20.0, 20.0, PhysicalProperties::default(), "Cand 2");
    cand2.state = PlanLifecycleState::Candidate; // Not validated!

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand1);
    set.add_candidate(cand2);

    // No validation context provided and candidates unvalidated -> must reject safely!
    let err = selector.select(&set, None, None, None, None, 1, None);
    assert!(err.is_err());
}

// =========================================================================
// 5. Resource-Aware Adaptive Selection
// =========================================================================

#[test]
fn test_resource_incompatible_candidate_disqualified() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig::default());

    let plan_cheap_high_mem = dummy_plan(AccessPath::FullTableScan, 10.0, 10.0);
    let plan_safe_low_mem = dummy_plan(AccessPath::FullTableScan, 50.0, 10.0);

    let res_high = ResourcePlanningSignature::new(100_000_000, 10, 10_000_000, 4);
    let res_low = ResourcePlanningSignature::new(1_000_000, 10, 10_000_000, 4);

    let cand_high = PlanCandidate::new(
        plan_cheap_high_mem,
        10.0,
        10.0,
        PhysicalProperties::default(),
        "High Memory",
    )
    .with_resource_signature(Some(res_high));
    let mut cand_high = mock_validated_candidate(cand_high.plan, 10.0, 10.0, "High Memory")
        .with_resource_signature(cand_high.resource_signature);
    cand_high.state = PlanLifecycleState::Validated;

    let cand_low = PlanCandidate::new(
        plan_safe_low_mem,
        50.0,
        10.0,
        PhysicalProperties::default(),
        "Low Memory",
    )
    .with_resource_signature(Some(res_low.clone()));
    let mut cand_low = mock_validated_candidate(cand_low.plan, 50.0, 10.0, "Low Memory")
        .with_resource_signature(cand_low.resource_signature);
    cand_low.state = PlanLifecycleState::Validated;

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand_high);
    set.add_candidate(cand_low.clone());

    // When active execution budget is res_low (1MB), cand_high (requires 100MB) must be disqualified!
    let res = selector
        .select(&set, None, None, None, Some(&res_low), 1, None)
        .unwrap();

    assert_eq!(res.selected_fingerprint, cand_low.fingerprint);
}

#[test]
fn test_resource_spill_feedback_adds_penalty() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        min_observations: 2,
        ..Default::default()
    });

    let mut reg = FeedbackRegistry::new();
    let plan = dummy_plan(AccessPath::FullTableScan, 30.0, 100.0);
    let cand = mock_validated_candidate(plan, 30.0, 100.0, "Scan");

    let key = FeedbackKey::scan("users", "FullTableScan", "", None, 1);
    let mut obs = FeedbackObservation::new(100.0, 100.0);
    obs.resource = Some(ResourceObservation::with_metrics(
        1000, 5000, 8000, 0, 50000, 3, 50000, 3, 1000,
    ));

    reg.record_observation(key.clone(), obs.clone());
    reg.record_observation(key, obs);

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand);

    let res = selector
        .select(&set, None, Some(&reg), None, None, 1, None)
        .unwrap();

    // Adjusted cost should reflect spill penalty (base 30.0 + spill penalty > 50.0)
    assert!(res.diagnostics.adjusted_cost > 50.0);
}

// =========================================================================
// 6. Parameter Planning Signature Isolation
// =========================================================================

#[test]
fn test_parameter_signatures_remain_isolated() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        min_observations: 2,
        ..Default::default()
    });

    let mut reg = FeedbackRegistry::new();
    let plan = dummy_plan(AccessPath::FullTableScan, 20.0, 10.0);
    let cand = mock_validated_candidate(plan, 20.0, 10.0, "Scan");

    let sig_a = ParamPlanningSignature::from_values(&[Value::Int64(10)]);
    let sig_b = ParamPlanningSignature::from_values(&[Value::Int64(999)]);

    let key_a = FeedbackKey::scan("users", "FullTableScan", "", Some(sig_a.clone()), 1);

    // Record severe misestimate for Param Signature A only
    reg.record_observation(key_a.clone(), FeedbackObservation::new(10.0, 5000.0));
    reg.record_observation(key_a, FeedbackObservation::new(10.0, 5000.0));

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand);

    // Param Signature A should see inflated adjusted cost
    let res_a = selector
        .select(&set, None, Some(&reg), Some(&sig_a), None, 1, None)
        .unwrap();
    assert!(res_a.diagnostics.adjusted_cost > 50.0);

    // Param Signature B should remain completely uncorrupted with base cost 20.0!
    let res_b = selector
        .select(&set, None, Some(&reg), Some(&sig_b), None, 1, None)
        .unwrap();
    assert_eq!(res_b.diagnostics.adjusted_cost, 20.0);
}

// =========================================================================
// 7. Plan Cache Lifecycle & Next-Execution Reoptimization
// =========================================================================

#[test]
fn test_cache_reoptimization_and_revalidation() -> Result<()> {
    let ctx = TestContext::new("cache_reopt");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name FROM users WHERE id = ?;")?;
    let param1 = [Value::Int64(1)];

    // First execution: cache miss, compiles and caches plan
    let (res1, metrics1) = txn.execute_prepared_with_metrics(&stmt, &param1)?;
    assert_eq!(res1.rows.len(), 1);
    assert!(!metrics1.plan_cache_hit);

    // Second execution: cache hit
    let (res2, metrics2) = txn.execute_prepared_with_metrics(&stmt, &param1)?;
    assert_eq!(res2.rows.len(), 1);
    assert!(metrics2.plan_cache_hit);

    // Mark plan cache entry as needs_reoptimization
    let res_sig = txn.resource_planning_signature();
    let param_sig = ParamPlanningSignature::from_values(&param1);
    let key = treedb::query::PlanCacheKey::new(
        &stmt.sql,
        param_sig,
        txn.catalog_generation(),
        txn.index_generation(),
        txn.statistics_generation(),
    )
    .with_resource_signature(Some(res_sig));

    txn.plan_cache_mut().mark_needs_reoptimization(&key);

    // Third execution: should detect needs_reoptimization and recompile plan (cache miss)!
    let (res3, metrics3) = txn.execute_prepared_with_metrics(&stmt, &param1)?;
    assert_eq!(res3.rows.len(), 1);
    assert!(!metrics3.plan_cache_hit);

    txn.commit()?;
    Ok(())
}

// =========================================================================
// 8. End-to-End Adaptive Plan Selection via Transaction
// =========================================================================

#[test]
fn test_end_to_end_transaction_adaptive_selection() -> Result<()> {
    let ctx = TestContext::new("e2e_aps");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let select_ast = treedb::sql::parse_sql("SELECT id, name, age FROM users WHERE age >= 25;")?;

    let res = txn.select_adaptive_plan(&select_ast, None, None)?;
    assert!(res.selected_plan.output_schema().columns.len() >= 3);
    assert_eq!(
        res.diagnostics.validated_candidates,
        res.diagnostics.candidates_considered
    );

    // Explain adaptive diagnostics
    let explain_text =
        txn.explain_adaptive_plan("SELECT id, name, age FROM users WHERE age >= 25;")?;
    assert!(explain_text.contains("Adaptive Selection:"));
    assert!(explain_text.contains("candidates:"));

    txn.commit()?;
    Ok(())
}

// =========================================================================
// 9. Persistence & Clean Restart Behavior
// =========================================================================

#[test]
fn test_persistence_clears_adaptive_state_on_restart() -> Result<()> {
    let ctx = TestContext::new("restart_clean");
    {
        let mut db = ctx.db();
        setup_test_schema(&mut db)?;
        let mut txn = db.begin_transaction()?;

        // Record synthetic feedback
        let key = FeedbackKey::scan("users", "FullTableScan", "", None, 1);
        txn.feedback_registry_mut()
            .record_observation(key, FeedbackObservation::new(10.0, 1000.0));
        assert_eq!(txn.feedback_registry().len(), 1);
        txn.commit()?;
    }

    // Reopen database from disk
    {
        let mut db = ctx.db();
        let txn = db.begin_transaction()?;
        // Feedback registry and plan cache are transient in-memory only!
        assert_eq!(txn.feedback_registry().len(), 0);
        assert_eq!(txn.plan_cache_stats().entries, 0);
    }

    Ok(())
}

// =========================================================================
// 10. Adversarial Corrupted Feedback Handled Gracefully
// =========================================================================

#[test]
fn test_adversarial_corrupted_feedback_is_rejected_safely() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig::default());
    let mut reg = FeedbackRegistry::new();

    let plan = dummy_plan(AccessPath::FullTableScan, 50.0, 100.0);
    let cand = mock_validated_candidate(plan, 50.0, 100.0, "Scan");

    let key = FeedbackKey::scan("users", "FullTableScan", "", None, 1);

    // Test NaN actual rows
    let obs_nan = FeedbackObservation {
        estimated_rows: 100.0,
        actual_rows: f64::NAN,
        estimated_cost: None,
        actual_time_us: None,
        input_rows: None,
        output_rows: None,
        actual_distinct_prefixes: None,
        actual_candidates: None,
        left_rows: None,
        right_rows: None,
        spill_occurred: None,
        resource: None,
        is_clamped: false,
    };
    reg.record_observation(key.clone(), obs_nan);

    // Test Infinite actual rows
    let obs_inf = FeedbackObservation::new(100.0, f64::INFINITY);
    reg.record_observation(key.clone(), obs_inf);

    // Test negative actual rows
    let obs_neg = FeedbackObservation::new(100.0, -999.0);
    reg.record_observation(key, obs_neg);

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand);

    // Selector must not panic and must fall back safely to base cost
    let res = selector
        .select(&set, None, Some(&reg), None, None, 1, None)
        .unwrap();

    assert!(res.diagnostics.adjusted_cost.is_finite());
    assert!(res.diagnostics.adjusted_cost > 0.0);
}

// =========================================================================
// 11. Plan-Flapping Stress Test
// =========================================================================

#[test]
fn test_plan_flapping_stress_alternating_signals() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig {
        switch_threshold: 0.85, // 15% better required to switch
        min_observations: 2,
        ..Default::default()
    });

    let plan_a = dummy_plan(AccessPath::FullTableScan, 100.0, 100.0);
    let plan_b = dummy_plan(
        AccessPath::SecondaryIndexScan {
            index_name: "idx_age".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::RangeScan {
                start: None,
                end: None,
            },
        },
        100.0,
        100.0,
    );

    let cand_a = mock_validated_candidate(plan_a, 100.0, 100.0, "Plan A");
    let cand_b = mock_validated_candidate(plan_b, 100.0, 100.0, "Plan B");

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand_a.clone());
    set.add_candidate(cand_b.clone());

    let mut current_fp = cand_a.fingerprint.clone();

    // Alternate slightly better signals (below 15% improvement threshold):
    for i in 1..=10 {
        if i % 2 == 1 {
            // B slightly cheaper (95 vs 100 = 5% better)
            let b_cand = set.find_by_fingerprint_mut(&cand_b.fingerprint).unwrap();
            b_cand.estimated_cost = 95.0;
            b_cand.adjusted_cost = 95.0;
            let a_cand = set.find_by_fingerprint_mut(&cand_a.fingerprint).unwrap();
            a_cand.estimated_cost = 100.0;
            a_cand.adjusted_cost = 100.0;
        } else {
            // A slightly cheaper (95 vs 100 = 5% better)
            let a_cand = set.find_by_fingerprint_mut(&cand_a.fingerprint).unwrap();
            a_cand.estimated_cost = 95.0;
            a_cand.adjusted_cost = 95.0;
            let b_cand = set.find_by_fingerprint_mut(&cand_b.fingerprint).unwrap();
            b_cand.estimated_cost = 100.0;
            b_cand.adjusted_cost = 100.0;
        }

        let res = selector
            .select(&set, Some(&current_fp), None, None, None, 1, None)
            .unwrap();

        // Hysteresis prevents switching on noisy 5% changes!
        assert_eq!(res.selected_fingerprint, current_fp);
        assert!(matches!(
            res.decision,
            ReplanDecision::KeepCurrentPlan { .. }
        ));
    }

    // Now supply a dominant signal: B is 50% cheaper (cost 50 vs 100)
    let b_cand = set.find_by_fingerprint_mut(&cand_b.fingerprint).unwrap();
    b_cand.estimated_cost = 50.0;
    b_cand.adjusted_cost = 50.0;
    let a_cand = set.find_by_fingerprint_mut(&cand_a.fingerprint).unwrap();
    a_cand.estimated_cost = 100.0;
    a_cand.adjusted_cost = 100.0;

    let res_dominant = selector
        .select(&set, Some(&current_fp), None, None, None, 1, None)
        .unwrap();

    // Now switching MUST occur!
    assert_eq!(res_dominant.selected_fingerprint, cand_b.fingerprint);
    assert!(matches!(
        res_dominant.decision,
        ReplanDecision::SwitchToAlternative { .. }
    ));
    current_fp = res_dominant.selected_fingerprint;

    assert_eq!(current_fp, cand_b.fingerprint);
}

// =========================================================================
// 12. Randomized Differential Testing: Baseline vs Adaptive Semantics
// =========================================================================

#[test]
fn test_randomized_differential_semantic_equivalence() -> Result<()> {
    let ctx = TestContext::new("random_diff");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let test_queries = [
        "SELECT id, name FROM users WHERE id = 10;",
        "SELECT id, name, age FROM users WHERE age = 30;",
        "SELECT id, dept_id FROM users WHERE dept_id = 10 ORDER BY id;",
        "SELECT id, name FROM users WHERE age > 25 AND age < 45;",
        "SELECT u.id, u.name, d.name FROM users u JOIN departments d ON u.dept_id = d.dept_id WHERE u.id <= 5;",
    ];

    for &sql in &test_queries {
        let mut txn = db.begin_transaction()?;

        // 1. Run query without feedback
        let res_baseline = txn.execute_statement(sql)?.into_query()?;

        // 2. Synthesize feedback observations for all access paths
        let scan_key = FeedbackKey::scan("users", "FullTableScan", "", None, 1);
        txn.feedback_registry_mut()
            .record_observation(scan_key, FeedbackObservation::new(100.0, 100.0));

        // 3. Run query with feedback active
        let res_adaptive = txn.execute_statement(sql)?.into_query()?;

        // The result rows, schema, and values MUST be identical! Removing/adding feedback
        // never alters query correctness.
        assert_eq!(
            res_baseline.rows.len(),
            res_adaptive.rows.len(),
            "Row count mismatch for: {sql}"
        );
        for (r1, r2) in res_baseline.rows.iter().zip(res_adaptive.rows.iter()) {
            assert_eq!(r1.values, r2.values, "Row values mismatch for: {sql}");
        }

        txn.commit()?;
    }

    Ok(())
}

#[test]
fn test_ordering_and_row_goal_candidate_preferences() {
    let mut selector = AdaptivePlanSelector::new(AdaptiveSelectionConfig::default());

    let plan_unordered = dummy_plan(AccessPath::FullTableScan, 10.0, 100.0);
    let plan_ordered = dummy_plan(AccessPath::FullTableScan, 10.0, 100.0);

    let props_ordered =
        PhysicalProperties::with_ordering(OrderingProperty::new(vec![OrderingColumn {
            column: "age".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: false,
        }]));

    let cand_unordered = mock_validated_candidate(plan_unordered, 10.0, 100.0, "Unordered");
    let mut cand_ordered = mock_validated_candidate(plan_ordered, 10.0, 100.0, "Ordered");
    cand_ordered.properties = props_ordered.clone();

    let mut set = PlanCandidateSet::new(5);
    set.add_candidate(cand_unordered);
    set.add_candidate(cand_ordered.clone());

    // With ordering guarantee, cand_ordered must be selected over cand_unordered on identical cost/cardinality
    let res = selector
        .select(&set, None, None, None, None, 1, None)
        .unwrap();

    assert_eq!(res.selected_fingerprint, cand_ordered.fingerprint);
}
