use std::collections::BTreeSet;
use std::path::PathBuf;
use tempfile::TempDir;

use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::expr::{AggregateExpr, BinaryOp, Expr};
use treedb::query::external_multi_index::MultiIndexExecutionMode;
use treedb::query::fingerprint::PHYSICAL_PLAN_FINGERPRINT_VERSION;
use treedb::query::operator::SortDirection;
use treedb::query::optimizer::{
    AccessPath, AggregateStrategy, JoinStrategy, MultiIndexChildScan, MultiIndexStrategy,
    PhysicalJoin, PhysicalPlan, PhysicalTableScan, SecondaryScanType,
};
use treedb::query::physical_properties::RowGoal;
use treedb::query::plan_cache::{PlanCache, PlanCacheKey, PlanValidity};
use treedb::query::runtime_filter::{RuntimeFilterPlan, RuntimeFilterScope, RuntimeFilterType};
use treedb::query::{
    FeedbackEntry, FeedbackKey, FeedbackObservation, LogicalOp, OptimizerMemo, SortKey,
};
use treedb::sql::ast::JoinType;
use treedb::IndexId;
use treedb::TreeDb;

struct TestContext {
    _temp_dir: TempDir,
    db_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join(format!("{name}.treedb"));
        Self {
            _temp_dir: temp_dir,
            db_path,
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

fn setup_test_schema(db: &mut TreeDb) -> Result<()> {
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, email VARCHAR, age BIGINT, score DOUBLE);",
    )?;
    tx.execute_statement("CREATE INDEX idx_users_email ON users (email);")?;
    tx.execute_statement("CREATE INDEX idx_users_age_score ON users (age, score);")?;

    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, amount DOUBLE, status VARCHAR);",
    )?;
    tx.execute_statement("CREATE INDEX idx_orders_user ON orders (user_id);")?;

    tx.execute_statement("INSERT INTO users VALUES (1, 'Alice', 'alice@test.com', 25, 95.5);")?;
    tx.execute_statement("INSERT INTO users VALUES (2, 'Bob', 'bob@test.com', 30, 80.0);")?;
    tx.execute_statement("INSERT INTO users VALUES (3, 'Charlie', 'charlie@test.com', 35, 88.0);")?;

    tx.execute_statement("INSERT INTO orders VALUES (101, 1, 150.0, 'shipped');")?;
    tx.execute_statement("INSERT INTO orders VALUES (102, 1, 200.0, 'completed');")?;
    tx.execute_statement("INSERT INTO orders VALUES (103, 2, 50.0, 'pending');")?;

    tx.commit()?;
    Ok(())
}

fn create_dummy_base_scan(table_name: &str, access_path: AccessPath) -> PhysicalTableScan {
    PhysicalTableScan {
        table_name: table_name.to_string(),
        table_alias: None,
        access_path,
        residual_filter: None,
        estimated_rows: 100.0,
        cost: 10.0,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        early_termination_possible: false,
        required_columns: Vec::new(),
        pruned_columns: Vec::new(),
        covered_columns: Vec::new(),
        late_materialization: false,
        index_covered_filter: None,
        base_record_filter: None,
    }
}

fn create_empty_plan(table_name: &str, access_path: AccessPath) -> PhysicalPlan {
    PhysicalPlan {
        ctes: Vec::new(),
        base_table: create_dummy_base_scan(table_name, access_path),
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
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        row_goal_early_termination: false,
        total_cost: 10.0,
        estimated_rows: 100.0,
    }
}

// ============================================================================
// 1. Basic Identity and Determinism Tests
// ============================================================================

#[test]
fn test_basic_identity_determinism_and_repetition() {
    let plan1 = create_empty_plan("users", AccessPath::FullTableScan);
    let plan2 = create_empty_plan("users", AccessPath::FullTableScan);

    let fp1 = plan1.fingerprint();
    let fp2 = plan2.fingerprint();

    // Invariant: Identical plan produces identical fingerprint
    assert_eq!(fp1, fp2);
    assert_eq!(fp1.version(), PHYSICAL_PLAN_FINGERPRINT_VERSION);
    assert_eq!(fp1.digest(), fp2.digest());
    assert_eq!(fp1.to_hex(), fp2.to_hex());
    assert_eq!(fp1.canonical(), fp2.canonical());

    // Invariant: Repeated generation produces identical result
    for _ in 0..10 {
        let rep = plan1.fingerprint();
        assert_eq!(rep, fp1);
        assert_eq!(rep.digest(), fp1.digest());
    }

    // Invariant: Hash and Ord consistency
    let mut set = BTreeSet::new();
    set.insert(fp1.clone());
    assert!(set.contains(&fp2));
}

#[test]
fn test_metadata_ordering_commutativity() {
    let mut plan1 = create_empty_plan("users", AccessPath::FullTableScan);
    plan1.base_table.required_columns = vec!["id".into(), "email".into(), "name".into()];
    plan1.base_table.covered_columns = vec!["col_b".into(), "col_a".into()];
    plan1.base_table.pruned_columns = vec!["z".into(), "y".into(), "x".into()];

    let mut plan2 = create_empty_plan("users", AccessPath::FullTableScan);
    plan2.base_table.required_columns = vec!["email".into(), "name".into(), "id".into()];
    plan2.base_table.covered_columns = vec!["col_a".into(), "col_b".into()];
    plan2.base_table.pruned_columns = vec!["x".into(), "z".into(), "y".into()];

    let fp1 = plan1.fingerprint();
    let fp2 = plan2.fingerprint();

    // Set-like column membership must produce identical fingerprint regardless of insertion order
    assert_eq!(fp1, fp2);
    assert_eq!(fp1.canonical(), fp2.canonical());
}

// ============================================================================
// 2. Access Path Distinctions
// ============================================================================

#[test]
fn test_access_path_distinctions() {
    let plan_full = create_empty_plan("users", AccessPath::FullTableScan);
    let plan_pk_point = create_empty_plan(
        "users",
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(42),
        },
    );
    let plan_pk_range = create_empty_plan(
        "users",
        AccessPath::PrimaryRangeScan {
            start: Some(Value::Int64(10)),
            end: Some(Value::Int64(50)),
        },
    );
    let plan_sec_point = create_empty_plan(
        "users",
        AccessPath::SecondaryIndexScan {
            index_name: "idx_email".into(),
            index_id: IndexId(1),
            scan_type: SecondaryScanType::PointLookup(Value::String("alice@test.com".into())),
        },
    );
    let plan_sec_other_idx = create_empty_plan(
        "users",
        AccessPath::SecondaryIndexScan {
            index_name: "idx_email_backup".into(),
            index_id: IndexId(2),
            scan_type: SecondaryScanType::PointLookup(Value::String("alice@test.com".into())),
        },
    );
    let plan_index_only = create_empty_plan(
        "users",
        AccessPath::IndexOnlyScan {
            index_name: "idx_email".into(),
            index_id: IndexId(1),
            scan_type: SecondaryScanType::PointLookup(Value::String("alice@test.com".into())),
            covered_columns: vec!["email".into(), "id".into()],
        },
    );

    let fp_full = plan_full.fingerprint();
    let fp_pk_point = plan_pk_point.fingerprint();
    let fp_pk_range = plan_pk_range.fingerprint();
    let fp_sec_point = plan_sec_point.fingerprint();
    let fp_sec_other_idx = plan_sec_other_idx.fingerprint();
    let fp_index_only = plan_index_only.fingerprint();

    assert_ne!(fp_full, fp_pk_point);
    assert_ne!(fp_pk_point, fp_pk_range);
    assert_ne!(fp_full, fp_sec_point);
    assert_ne!(fp_sec_point, fp_sec_other_idx);
    assert_ne!(fp_sec_point, fp_index_only);
}

#[test]
fn test_multi_index_scan_commutativity_and_distinctions() {
    let child1 = MultiIndexChildScan {
        index_name: "idx_a".into(),
        index_id: IndexId(1),
        scan_type: SecondaryScanType::PointLookup(Value::Int64(10)),
        cost: 5.0,
        estimated_rows: 20.0,
    };
    let child2 = MultiIndexChildScan {
        index_name: "idx_b".into(),
        index_id: IndexId(2),
        scan_type: SecondaryScanType::PointLookup(Value::Int64(20)),
        cost: 5.0,
        estimated_rows: 20.0,
    };

    let plan_inter1 = create_empty_plan(
        "users",
        AccessPath::MultiIndexScan {
            strategy: MultiIndexStrategy::Intersection,
            child_scans: vec![child1.clone(), child2.clone()],
            execution_mode: MultiIndexExecutionMode::InMemory,
        },
    );
    let plan_inter2 = create_empty_plan(
        "users",
        AccessPath::MultiIndexScan {
            strategy: MultiIndexStrategy::Intersection,
            child_scans: vec![child2.clone(), child1.clone()],
            execution_mode: MultiIndexExecutionMode::InMemory,
        },
    );
    let plan_union = create_empty_plan(
        "users",
        AccessPath::MultiIndexScan {
            strategy: MultiIndexStrategy::Union,
            child_scans: vec![child1.clone(), child2.clone()],
            execution_mode: MultiIndexExecutionMode::InMemory,
        },
    );
    let plan_inter_external = create_empty_plan(
        "users",
        AccessPath::MultiIndexScan {
            strategy: MultiIndexStrategy::Intersection,
            child_scans: vec![child1, child2],
            execution_mode: MultiIndexExecutionMode::External,
        },
    );

    // MultiIndex intersection is commutative on sets of record IDs: child order must produce same fingerprint
    assert_eq!(plan_inter1.fingerprint(), plan_inter2.fingerprint());

    // Intersection vs Union must differ
    assert_ne!(plan_inter1.fingerprint(), plan_union.fingerprint());

    // InMemory vs Spilled External execution mode must differ
    assert_ne!(plan_inter1.fingerprint(), plan_inter_external.fingerprint());
}

// ============================================================================
// 3. Ordering Property Distinctions
// ============================================================================

#[test]
fn test_ordering_properties_distinctions() {
    let mut plan_asc = create_empty_plan("users", AccessPath::FullTableScan);
    plan_asc.order_by = vec![SortKey {
        expression: Expr::Column("age".into()),
        direction: SortDirection::Ascending,
        nulls_first: Some(true),
    }];

    let mut plan_desc = create_empty_plan("users", AccessPath::FullTableScan);
    plan_desc.order_by = vec![SortKey {
        expression: Expr::Column("age".into()),
        direction: SortDirection::Descending,
        nulls_first: Some(true),
    }];

    let mut plan_nulls_last = create_empty_plan("users", AccessPath::FullTableScan);
    plan_nulls_last.order_by = vec![SortKey {
        expression: Expr::Column("age".into()),
        direction: SortDirection::Ascending,
        nulls_first: Some(false),
    }];

    let mut plan_ab = create_empty_plan("users", AccessPath::FullTableScan);
    plan_ab.order_by = vec![
        SortKey {
            expression: Expr::Column("age".into()),
            direction: SortDirection::Ascending,
            nulls_first: Some(true),
        },
        SortKey {
            expression: Expr::Column("name".into()),
            direction: SortDirection::Ascending,
            nulls_first: Some(true),
        },
    ];

    let mut plan_ba = create_empty_plan("users", AccessPath::FullTableScan);
    plan_ba.order_by = vec![
        SortKey {
            expression: Expr::Column("name".into()),
            direction: SortDirection::Ascending,
            nulls_first: Some(true),
        },
        SortKey {
            expression: Expr::Column("age".into()),
            direction: SortDirection::Ascending,
            nulls_first: Some(true),
        },
    ];

    assert_ne!(plan_asc.fingerprint(), plan_desc.fingerprint());
    assert_ne!(plan_asc.fingerprint(), plan_nulls_last.fingerprint());
    assert_ne!(plan_ab.fingerprint(), plan_ba.fingerprint());
    assert_ne!(plan_ab.fingerprint(), plan_asc.fingerprint());
}

// ============================================================================
// 4. Row Goal and Pagination Distinctions
// ============================================================================

#[test]
fn test_row_goal_and_pagination_distinctions() {
    let mut plan_lim10 = create_empty_plan("users", AccessPath::FullTableScan);
    plan_lim10.limit = Some(10);

    let mut plan_lim20 = create_empty_plan("users", AccessPath::FullTableScan);
    plan_lim20.limit = Some(20);

    let mut plan_off10 = create_empty_plan("users", AccessPath::FullTableScan);
    plan_off10.limit = Some(10);
    plan_off10.offset = Some(10);

    let mut plan_rg1 = create_empty_plan("users", AccessPath::FullTableScan);
    plan_rg1.row_goal = Some(RowGoal::new(10, 10));

    let mut plan_rg2 = create_empty_plan("users", AccessPath::FullTableScan);
    plan_rg2.row_goal = Some(RowGoal::new(50, 10));

    let mut plan_early_term = create_empty_plan("users", AccessPath::FullTableScan);
    plan_early_term.row_goal_early_termination = true;

    assert_ne!(plan_lim10.fingerprint(), plan_lim20.fingerprint());
    assert_ne!(plan_lim10.fingerprint(), plan_off10.fingerprint());
    assert_ne!(plan_rg1.fingerprint(), plan_rg2.fingerprint());
    assert_ne!(plan_lim10.fingerprint(), plan_early_term.fingerprint());
}

// ============================================================================
// 5. Join Semantics Distinctions
// ============================================================================

#[test]
fn test_join_semantics_distinctions() {
    let make_join_plan = |j_type: JoinType, strat: JoinStrategy, left_first: bool| {
        let mut plan = create_empty_plan("users", AccessPath::FullTableScan);
        let right_scan = create_dummy_base_scan(
            if left_first { "orders" } else { "users" },
            AccessPath::FullTableScan,
        );
        plan.joins.push(PhysicalJoin {
            join_type: j_type,
            strategy: strat,
            index_name: None,
            on: Expr::Binary {
                left: Box::new(Expr::Column("users.id".into())),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Column("orders.user_id".into())),
            },
            right: right_scan,
            left_sort: false,
            right_sort: false,
            cost: 20.0,
            estimated_rows: 50.0,
            row_goal: None,
            estimated_rows_examined: None,
            estimated_qualifying_rows: None,
            early_termination_possible: false,
            early_stop_phase: None,
            runtime_filter: None,
        });
        plan
    };

    let p_inner = make_join_plan(JoinType::Inner, JoinStrategy::Hash, true);
    let p_left = make_join_plan(JoinType::Left, JoinStrategy::Hash, true);
    let p_right = make_join_plan(JoinType::Right, JoinStrategy::Hash, true);
    let p_full = make_join_plan(JoinType::Full, JoinStrategy::Hash, true);
    let p_semi = make_join_plan(JoinType::Semi, JoinStrategy::Hash, true);
    let p_anti = make_join_plan(JoinType::Anti, JoinStrategy::Hash, true);

    let p_merge = make_join_plan(JoinType::Inner, JoinStrategy::Merge, true);
    let p_swapped = make_join_plan(JoinType::Inner, JoinStrategy::Hash, false);

    assert_ne!(p_inner.fingerprint(), p_left.fingerprint());
    assert_ne!(p_left.fingerprint(), p_right.fingerprint());
    assert_ne!(p_semi.fingerprint(), p_anti.fingerprint());
    assert_ne!(p_inner.fingerprint(), p_full.fingerprint());
    assert_ne!(p_inner.fingerprint(), p_merge.fingerprint());
    assert_ne!(p_inner.fingerprint(), p_swapped.fingerprint());
}

// ============================================================================
// 6. Aggregation Distinctions
// ============================================================================

#[test]
fn test_aggregation_distinctions() {
    let mut plan_hash = create_empty_plan("users", AccessPath::FullTableScan);
    plan_hash.group_by = vec![Expr::Column("age".into())];
    plan_hash.aggregates = vec![AggregateExpr::count_all()];
    plan_hash.aggregate_strategy = Some(AggregateStrategy::Hash);

    let mut plan_stream = create_empty_plan("users", AccessPath::FullTableScan);
    plan_stream.group_by = vec![Expr::Column("age".into())];
    plan_stream.aggregates = vec![AggregateExpr::count_all()];
    plan_stream.aggregate_strategy = Some(AggregateStrategy::Streaming {
        sort_required: false,
    });

    let mut plan_ext = create_empty_plan("users", AccessPath::FullTableScan);
    plan_ext.group_by = vec![Expr::Column("age".into())];
    plan_ext.aggregates = vec![AggregateExpr::count_all()];
    plan_ext.aggregate_strategy = Some(AggregateStrategy::External);

    let mut plan_diff_agg = create_empty_plan("users", AccessPath::FullTableScan);
    plan_diff_agg.group_by = vec![Expr::Column("age".into())];
    plan_diff_agg.aggregates = vec![AggregateExpr::sum(Expr::Column("score".into()))];
    plan_diff_agg.aggregate_strategy = Some(AggregateStrategy::Hash);

    assert_ne!(plan_hash.fingerprint(), plan_stream.fingerprint());
    assert_ne!(plan_hash.fingerprint(), plan_ext.fingerprint());
    assert_ne!(plan_hash.fingerprint(), plan_diff_agg.fingerprint());
}

// ============================================================================
// 7. Expressions and Literal Determinism Tests
// ============================================================================

#[test]
fn test_expression_and_literal_edge_cases() {
    // Parameter position vs value
    let mut p_param1 = create_empty_plan("users", AccessPath::FullTableScan);
    p_param1.post_join_filter = Some(Expr::Parameter {
        index: 1,
        value: Value::Int64(100),
    });

    let mut p_param1_diff_val = create_empty_plan("users", AccessPath::FullTableScan);
    p_param1_diff_val.post_join_filter = Some(Expr::Parameter {
        index: 1,
        value: Value::Int64(999),
    });

    let mut p_param2 = create_empty_plan("users", AccessPath::FullTableScan);
    p_param2.post_join_filter = Some(Expr::Parameter {
        index: 2,
        value: Value::Int64(100),
    });

    // Parameter values must NOT change structural physical fingerprint
    assert_eq!(p_param1.fingerprint(), p_param1_diff_val.fingerprint());
    // Parameter positional index $1 vs $2 MUST change fingerprint
    assert_ne!(p_param1.fingerprint(), p_param2.fingerprint());

    // Operand order preservation (a = b vs b = a)
    let mut p_ab = create_empty_plan("users", AccessPath::FullTableScan);
    p_ab.post_join_filter = Some(Expr::Binary {
        left: Box::new(Expr::Column("a".into())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("b".into())),
    });

    let mut p_ba = create_empty_plan("users", AccessPath::FullTableScan);
    p_ba.post_join_filter = Some(Expr::Binary {
        left: Box::new(Expr::Column("b".into())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("a".into())),
    });
    assert_ne!(p_ab.fingerprint(), p_ba.fingerprint());

    // Signed vs unsigned
    let mut p_i64 = create_empty_plan("users", AccessPath::FullTableScan);
    p_i64.post_join_filter = Some(Expr::Literal(Value::Int64(42)));

    let mut p_u64 = create_empty_plan("users", AccessPath::FullTableScan);
    p_u64.post_join_filter = Some(Expr::Literal(Value::UInt64(42)));

    assert_ne!(p_i64.fingerprint(), p_u64.fingerprint());

    // Floating-point edge cases
    let mut p_pos_zero = create_empty_plan("users", AccessPath::FullTableScan);
    p_pos_zero.post_join_filter = Some(Expr::Literal(Value::Float64(0.0)));

    let mut p_neg_zero = create_empty_plan("users", AccessPath::FullTableScan);
    p_neg_zero.post_join_filter = Some(Expr::Literal(Value::Float64(-0.0)));

    assert_ne!(p_pos_zero.fingerprint(), p_neg_zero.fingerprint());

    // NaN determinism
    let mut p_nan1 = create_empty_plan("users", AccessPath::FullTableScan);
    p_nan1.post_join_filter = Some(Expr::Literal(Value::Float64(f64::NAN)));

    let mut p_nan2 = create_empty_plan("users", AccessPath::FullTableScan);
    p_nan2.post_join_filter = Some(Expr::Literal(Value::Float64(f64::from_bits(
        0x7ff8000000000001,
    ))));

    assert_eq!(p_nan1.fingerprint(), p_nan2.fingerprint());
}

// ============================================================================
// 8. Runtime Filters Distinctions
// ============================================================================

#[test]
fn test_runtime_filters_distinctions() {
    let mut p_no_rf = create_empty_plan("users", AccessPath::FullTableScan);
    let right_scan = create_dummy_base_scan("orders", AccessPath::FullTableScan);
    p_no_rf.joins.push(PhysicalJoin {
        join_type: JoinType::Inner,
        strategy: JoinStrategy::Hash,
        index_name: None,
        on: Expr::Binary {
            left: Box::new(Expr::Column("users.id".into())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column("orders.user_id".into())),
        },
        right: right_scan.clone(),
        left_sort: false,
        right_sort: false,
        cost: 20.0,
        estimated_rows: 50.0,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        early_termination_possible: false,
        early_stop_phase: None,
        runtime_filter: None,
    });

    let mut p_exact_rf = p_no_rf.clone();
    p_exact_rf.joins[0].runtime_filter = Some(RuntimeFilterPlan {
        filter_type: RuntimeFilterType::ExactSet,
        scope: RuntimeFilterScope::Global,
        key_exprs: vec![Expr::Column("orders.user_id".into())],
    });

    let mut p_bloom_rf = p_no_rf.clone();
    p_bloom_rf.joins[0].runtime_filter = Some(RuntimeFilterPlan {
        filter_type: RuntimeFilterType::Bloom,
        scope: RuntimeFilterScope::Global,
        key_exprs: vec![Expr::Column("orders.user_id".into())],
    });

    assert_ne!(p_no_rf.fingerprint(), p_exact_rf.fingerprint());
    assert_ne!(p_exact_rf.fingerprint(), p_bloom_rf.fingerprint());
}

// ============================================================================
// 9. Cost and Cardinality Invariance Tests
// ============================================================================

#[test]
fn test_cost_cardinality_and_stats_invariance() {
    let mut plan1 = create_empty_plan("users", AccessPath::FullTableScan);
    plan1.total_cost = 15.0;
    plan1.estimated_rows = 50.0;
    plan1.estimated_rows_examined = Some(200.0);
    plan1.estimated_qualifying_rows = Some(50.0);
    plan1.logical_rewrites = vec!["PushdownFilter".into()];

    let mut plan2 = create_empty_plan("users", AccessPath::FullTableScan);
    plan2.total_cost = 888.88;
    plan2.estimated_rows = 9999.0;
    plan2.estimated_rows_examined = Some(10000.0);
    plan2.estimated_qualifying_rows = Some(9999.0);
    plan2.logical_rewrites = vec!["DecorrelateSubquery".into()];

    // Structural execution strategy is identical -> fingerprint MUST be identical
    assert_eq!(plan1.fingerprint(), plan2.fingerprint());
    assert_eq!(plan1.fingerprint().digest(), plan2.fingerprint().digest());
}

// ============================================================================
// 10. Memo Deduplication Integration Tests
// ============================================================================

#[test]
fn test_memo_deduplication_with_physical_fingerprints() {
    let mut memo = OptimizerMemo::default();
    let gid = memo
        .get_or_create_group(LogicalOp::Scan {
            table_name: "users".into(),
            table_alias: None,
            filter: None,
        })
        .unwrap();

    let plan_a = create_empty_plan("users", AccessPath::FullTableScan);
    let plan_b = create_empty_plan("users", AccessPath::FullTableScan);

    let added_a = memo
        .add_alternative(gid, plan_a, "TableScan baseline")
        .unwrap();
    assert!(added_a);

    // Identical physical execution strategy and description must be deduplicated within the group
    let added_b = memo
        .add_alternative(gid, plan_b, "TableScan baseline")
        .unwrap();
    assert!(!added_b);
    assert_eq!(memo.group(gid).unwrap().alternatives.len(), 1);

    // Different physical strategy must be accepted as distinct alternative
    let plan_idx = create_empty_plan(
        "users",
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1),
        },
    );
    let added_idx = memo
        .add_alternative(gid, plan_idx, "Primary key lookup")
        .unwrap();
    assert!(added_idx);
    assert_eq!(memo.group(gid).unwrap().alternatives.len(), 2);
}

// ============================================================================
// 11. Plan Cache Integration Tests
// ============================================================================

#[test]
fn test_plan_cache_integration() {
    let mut cache = PlanCache::new();
    let param_sig = treedb::query::ParamPlanningSignature::empty();
    let key = PlanCacheKey::new("SELECT * FROM users WHERE id = ?", param_sig, 1, 1, 1);
    let plan = create_empty_plan(
        "users",
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1),
        },
    );
    let fp = plan.fingerprint();

    let validity = PlanValidity::new(1, 1, 1, vec!["users".into()], vec![]);
    cache.insert(key.clone(), plan.clone(), validity);

    // Cached entry preserves deterministic physical plan fingerprint
    let entry = cache.get_entry(&key).expect("entry must exist in cache");
    assert_eq!(entry.fingerprint, fp);
    assert_eq!(cache.fingerprint(&key), Some(&fp));

    // Stale generation still rejects cached plan via validity check
    let retrieved = cache.get(&key, 2, 1, 1);
    assert!(retrieved.is_none());
}

// ============================================================================
// 12. Feedback Integration Tests
// ============================================================================

#[test]
fn test_feedback_integration() {
    let plan_full = create_empty_plan("users", AccessPath::FullTableScan);
    let plan_idx = create_empty_plan(
        "users",
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1),
        },
    );

    let fp_full = plan_full.fingerprint();
    let fp_idx = plan_idx.fingerprint();

    let key_full = FeedbackKey::scan("users", "FullScan", "true", None, 1)
        .with_physical_fingerprint(Some(fp_full.clone()));
    let key_idx = FeedbackKey::scan("users", "PrimaryLookup", "id = 1", None, 1)
        .with_physical_fingerprint(Some(fp_idx.clone()));

    assert_ne!(key_full, key_idx);
    assert_eq!(key_full.physical_fingerprint.as_ref().unwrap(), &fp_full);
    assert_eq!(key_idx.physical_fingerprint.as_ref().unwrap(), &fp_idx);

    let obs = FeedbackObservation::new(10.0, 50.0);
    let entry = FeedbackEntry::new(key_full.clone(), obs, 1);
    assert_eq!(entry.key, key_full);
}

// ============================================================================
// 13. EXPLAIN Diagnostics Integration Tests
// ============================================================================

#[test]
fn test_explain_diagnostics_integration() {
    let ctx = TestContext::new("explain_fp");
    let mut db = ctx.db();
    setup_test_schema(&mut db).unwrap();

    let sql = "SELECT id, name FROM users WHERE id = 1;";

    // Deterministic fingerprint generation
    let fp1 = db.explain_plan_fingerprint(sql).unwrap();
    let fp2 = db.explain_plan_fingerprint(sql).unwrap();
    assert_eq!(fp1, fp2);
    assert_eq!(fp1.version(), PHYSICAL_PLAN_FINGERPRINT_VERSION);

    // Detailed explanation includes the fingerprint header
    let diag = db.explain_sql_with_fingerprint(sql).unwrap();
    assert!(diag.contains("Physical Plan Fingerprint:"));
    assert!(diag.contains(&fp1.to_hex()));
}

// ============================================================================
// 14. Persistence and Recovery Invariance Tests
// ============================================================================

#[test]
fn test_persistence_and_recovery_invariance() {
    let ctx = TestContext::new("recovery_fp");
    let sql = "SELECT id, email FROM users WHERE email = 'alice@test.com';";

    let fp_before = {
        let mut db = ctx.db();
        setup_test_schema(&mut db).unwrap();
        db.explain_plan_fingerprint(sql).unwrap()
    };

    // Reopen and recover database
    let fp_after = {
        let mut db = ctx.db();
        db.explain_plan_fingerprint(sql).unwrap()
    };

    assert_eq!(fp_before, fp_after);
    assert_eq!(fp_before.digest(), fp_after.digest());
    assert_eq!(fp_before.canonical(), fp_after.canonical());
}

// ============================================================================
// 15. Randomized Differential Mutation Tests
// ============================================================================

#[test]
fn test_randomized_differential_mutations() {
    for i in 0..250 {
        let mut plan = create_empty_plan("table_a", AccessPath::FullTableScan);
        plan.limit = Some(10 + i);
        plan.offset = Some(i % 5);
        if i % 2 == 0 {
            plan.base_table.required_columns = vec!["col_x".into(), "col_y".into()];
        }
        if i % 3 == 0 {
            plan.order_by = vec![SortKey {
                expression: Expr::Column("col_x".into()),
                direction: if i % 6 == 0 {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                },
                nulls_first: Some(i % 4 == 0),
            }];
        }

        let base_fp = plan.fingerprint();

        // 1. Determinism
        assert_eq!(base_fp, plan.fingerprint());

        // 2. Safe metadata reordering
        let mut safe_reordered = plan.clone();
        if !safe_reordered.base_table.required_columns.is_empty() {
            safe_reordered.base_table.required_columns.reverse();
            assert_eq!(base_fp, safe_reordered.fingerprint());
        }

        // 3. Execution-semantic mutation: change LIMIT
        let mut mut_limit = plan.clone();
        mut_limit.limit = Some(9999 + i);
        assert_ne!(base_fp, mut_limit.fingerprint());

        // 4. Execution-semantic mutation: change access path
        let mut mut_path = plan.clone();
        mut_path.base_table.access_path = AccessPath::PrimaryPointLookup {
            key: Value::Int64(i as i64),
        };
        assert_ne!(base_fp, mut_path.fingerprint());
    }
}
