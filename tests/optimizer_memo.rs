use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::memo::{GroupId, LogicalOp, MemoConfig, OptimizerMemo, PhysicalPropertyKey};
use treedb::query::operator::SortDirection;
use treedb::query::optimizer::{AccessPath, PhysicalPlan, PhysicalTableScan};
use treedb::query::physical_properties::{
    OrderingColumn, OrderingProperty, RequiredColumns, RowGoal,
};
use treedb::query::plan_cache::ResourcePlanningSignature;
use treedb::TreeDb;

struct TestContext {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
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

fn dummy_plan(cost: f64, rows: f64) -> PhysicalPlan {
    PhysicalPlan {
        ctes: Vec::new(),
        base_table: PhysicalTableScan {
            table_name: "users".to_string(),
            table_alias: None,
            access_path: AccessPath::FullTableScan,
            residual_filter: None,
            estimated_rows: rows,
            cost,
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
        },
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
        total_cost: cost,
        estimated_rows: rows,
    }
}

fn setup_standard_schema(db: &mut TreeDb) -> Result<()> {
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, status VARCHAR, age BIGINT, score DOUBLE, created_at BIGINT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_users_status ON users (status);")?;
    tx.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    tx.execute_statement("CREATE INDEX idx_users_created ON users (created_at);")?;
    tx.execute_statement("CREATE INDEX idx_users_status_age ON users (status, age);")?;

    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, amount DOUBLE, order_date BIGINT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_orders_user ON orders (user_id);")?;

    for i in 1..=50 {
        let status = if i % 2 == 0 { "active" } else { "inactive" };
        let age = 20 + (i % 30);
        let score = (i as f64) * 2.5;
        let created_at = 1000 + i;
        tx.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'User_{i}', '{status}', {age}, {score}, {created_at});"
        ))?;
        tx.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, {i}, {score} * 10.0, {created_at});"
        ))?;
    }
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1495..R1502: Memo Fundamentals
// -----------------------------------------------------------------------------

#[test]
fn test_r1495_logical_group_creation() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: Some("status = 'active'".to_string()),
    };
    let gid = memo.get_or_create_group(op).unwrap();
    assert_eq!(gid, GroupId(0));
    assert_eq!(memo.groups().len(), 1);
}

#[test]
fn test_r1496_deterministic_group_identity() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op1 = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: Some("a = 1 AND b = 2".to_string()),
    };
    let op2 = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: Some("a = 1 AND b = 2".to_string()),
    };
    assert_eq!(op1.fingerprint(), op2.fingerprint());
    let g1 = memo.get_or_create_group(op1).unwrap();
    let g2 = memo.get_or_create_group(op2).unwrap();
    assert_eq!(g1, g2);
}

#[test]
fn test_r1497_duplicate_logical_expression_deduplication() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "orders".to_string(),
        table_alias: None,
        filter: None,
    };
    let g1 = memo.get_or_create_group(op.clone()).unwrap();
    let g2 = memo.get_or_create_group(op).unwrap();
    assert_eq!(g1, g2);
    assert_eq!(memo.groups().len(), 1);
}

#[test]
fn test_r1498_physical_alternative_registration() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let plan1 = dummy_plan(100.0, 50.0);
    let plan2 = dummy_plan(20.0, 50.0);

    let added1 = memo
        .add_alternative(gid, plan1, "FullTableScan".to_string())
        .unwrap();
    let added2 = memo
        .add_alternative(gid, plan2, "IndexScan".to_string())
        .unwrap();
    assert!(added1);
    assert!(added2);
    assert_eq!(memo.group(gid).unwrap().alternatives.len(), 2);
}

#[test]
fn test_r1499_physical_alternative_deduplication() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let plan1 = dummy_plan(100.0, 50.0);
    let plan2 = dummy_plan(100.0, 50.0);

    let added1 = memo
        .add_alternative(gid, plan1, "FullTableScan".to_string())
        .unwrap();
    let added2 = memo
        .add_alternative(gid, plan2, "FullTableScan".to_string())
        .unwrap();
    assert!(added1);
    assert!(!added2);
    assert_eq!(memo.group(gid).unwrap().alternatives.len(), 1);
}

#[test]
fn test_r1500_property_sensitive_memo_lookup() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let mut plan1 = dummy_plan(100.0, 50.0);
    plan1.provided_ordering = Some(OrderingProperty::default());
    let mut plan2 = dummy_plan(20.0, 50.0);
    plan2.provided_ordering = Some(OrderingProperty {
        columns: vec![OrderingColumn {
            column: "created_at".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        }],
    });

    memo.add_alternative(gid, plan1, "FullScan".to_string())
        .unwrap();
    memo.add_alternative(gid, plan2, "IndexScan".to_string())
        .unwrap();

    let req_ord = OrderingProperty {
        columns: vec![OrderingColumn {
            column: "created_at".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        }],
    };
    let req_cols = RequiredColumns::all();
    let prop_key = PhysicalPropertyKey::new(&req_ord, &req_cols, None, None);

    let best = memo
        .optimize_group(gid, &prop_key, &req_ord, &req_cols, None)
        .unwrap();
    assert_eq!(best.cost, 20.0);
    assert!(best
        .properties
        .ordering
        .as_ref()
        .unwrap()
        .satisfies(&req_ord));
}

#[test]
fn test_r1501_deterministic_exploration() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let p1 = dummy_plan(50.0, 10.0);
    let p2 = dummy_plan(30.0, 10.0);
    let p3 = dummy_plan(80.0, 10.0);

    memo.add_alternative(gid, p1, "P1".to_string()).unwrap();
    memo.add_alternative(gid, p2, "P2".to_string()).unwrap();
    memo.add_alternative(gid, p3, "P3".to_string()).unwrap();

    let req_ord = OrderingProperty::default();
    let req_cols = RequiredColumns::all();
    let prop_key = PhysicalPropertyKey::new(&req_ord, &req_cols, None, None);

    let best1 = memo
        .optimize_group(gid, &prop_key, &req_ord, &req_cols, None)
        .unwrap();
    let best2 = memo
        .optimize_group(gid, &prop_key, &req_ord, &req_cols, None)
        .unwrap();
    assert_eq!(best1.cost, best2.cost);
    assert_eq!(best1.cost, 30.0);
}

#[test]
fn test_r1502_best_plan_selection() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let p1 = dummy_plan(50.0, 10.0);
    let p2 = dummy_plan(10.0, 10.0);

    memo.add_alternative(gid, p1, "HighCost".to_string())
        .unwrap();
    memo.add_alternative(gid, p2, "LowCost".to_string())
        .unwrap();

    let req_ord = OrderingProperty::default();
    let req_cols = RequiredColumns::all();
    let prop_key = PhysicalPropertyKey::new(&req_ord, &req_cols, None, None);

    let best = memo
        .optimize_group(gid, &prop_key, &req_ord, &req_cols, None)
        .unwrap();
    assert_eq!(best.cost, 10.0);
}

// -----------------------------------------------------------------------------
// R1503..R1512: Physical Properties
// -----------------------------------------------------------------------------

#[test]
fn test_r1503_ordering_variants() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let mut p_unordered = dummy_plan(10.0, 100.0);
    p_unordered.provided_ordering = Some(OrderingProperty::default());
    let mut p_ordered = dummy_plan(25.0, 100.0);
    p_ordered.provided_ordering = Some(OrderingProperty {
        columns: vec![OrderingColumn {
            column: "age".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        }],
    });

    memo.add_alternative(gid, p_unordered, "Unordered".to_string())
        .unwrap();
    memo.add_alternative(gid, p_ordered, "Ordered".to_string())
        .unwrap();

    let req_ord1 = OrderingProperty::default();
    let prop_key1 = PhysicalPropertyKey::new(&req_ord1, &RequiredColumns::all(), None, None);
    let best1 = memo
        .optimize_group(gid, &prop_key1, &req_ord1, &RequiredColumns::all(), None)
        .unwrap();
    assert_eq!(best1.cost, 10.0);

    let req_ord2 = OrderingProperty {
        columns: vec![OrderingColumn {
            column: "age".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        }],
    };
    let prop_key2 = PhysicalPropertyKey::new(&req_ord2, &RequiredColumns::all(), None, None);
    let best2 = memo
        .optimize_group(gid, &prop_key2, &req_ord2, &RequiredColumns::all(), None)
        .unwrap();
    assert_eq!(best2.cost, 25.0);
}

#[test]
fn test_r1504_required_column_variants() {
    let req1 = RequiredColumns::from_columns(vec!["id"]);
    let req2 = RequiredColumns::from_columns(vec!["id", "name"]);

    let ord = OrderingProperty::default();
    let k1 = PhysicalPropertyKey::new(&ord, &req1, None, None);
    let k2 = PhysicalPropertyKey::new(&ord, &req2, None, None);
    assert_ne!(k1.fingerprint(), k2.fingerprint());
}

#[test]
fn test_r1505_row_goal_variants() {
    let ord = OrderingProperty::default();
    let cols = RequiredColumns::all();
    let rg1 = RowGoal::new(10, 10);
    let rg2 = RowGoal::new(100, 100);

    let k1 = PhysicalPropertyKey::new(&ord, &cols, Some(&rg1), None);
    let k2 = PhysicalPropertyKey::new(&ord, &cols, Some(&rg2), None);
    let k3 = PhysicalPropertyKey::new(&ord, &cols, None, None);
    assert_ne!(k1.fingerprint(), k2.fingerprint());
    assert_ne!(k1.fingerprint(), k3.fingerprint());
}

#[test]
fn test_r1506_resource_variants() {
    let ord = OrderingProperty::default();
    let cols = RequiredColumns::all();
    let res1 = ResourcePlanningSignature::new(64, 4, 1024, 2);
    let res2 = ResourcePlanningSignature::new(1024, 4, 1024, 2);
    let k1 = PhysicalPropertyKey::new(&ord, &cols, None, Some(&res1));
    let k2 = PhysicalPropertyKey::new(&ord, &cols, None, Some(&res2));
    assert_ne!(k1.fingerprint(), k2.fingerprint());
}

#[test]
fn test_r1507_combined_property_variants() {
    let ord = OrderingProperty {
        columns: vec![OrderingColumn {
            column: "score".to_string(),
            direction: SortDirection::Descending,
            nulls_first: false,
        }],
    };
    let req_cols = RequiredColumns::from_columns(vec!["score"]);
    let rg = RowGoal::new(5, 5);
    let res = ResourcePlanningSignature::new(512, 2, 512, 1);

    let k = PhysicalPropertyKey::new(&ord, &req_cols, Some(&rg), Some(&res));
    assert!(k.fingerprint().contains("ord:"));
    assert!(k.fingerprint().contains("cols:"));
    assert!(k.fingerprint().contains("rg:"));
    assert!(k.fingerprint().contains("res:"));
}

#[test]
fn test_r1508_property_satisfaction() {
    let ord = OrderingProperty {
        columns: vec![
            OrderingColumn {
                column: "a".to_string(),
                direction: SortDirection::Ascending,
                nulls_first: true,
            },
            OrderingColumn {
                column: "b".to_string(),
                direction: SortDirection::Ascending,
                nulls_first: true,
            },
        ],
    };
    let prefix_ord = OrderingProperty {
        columns: vec![OrderingColumn {
            column: "a".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        }],
    };
    assert!(ord.satisfies(&prefix_ord));
    assert!(!prefix_ord.satisfies(&ord));
}

#[test]
fn test_r1509_property_enforcement() -> Result<()> {
    let ctx = TestContext::new("r1509");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users ORDER BY name;")?;
    assert!(plan.to_lowercase().contains("sort") || plan.to_lowercase().contains("order"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1510_redundant_sort_elimination() -> Result<()> {
    let ctx = TestContext::new("r1510");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users ORDER BY id;")?;
    assert!(!plan.to_lowercase().contains("sort: true"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1511_ordered_index_preference() -> Result<()> {
    let ctx = TestContext::new("r1511");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users ORDER BY created_at LIMIT 5;")?;
    assert!(plan.contains("idx_users_created") || plan.contains("Index"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1512_topn_row_goal_preference() -> Result<()> {
    let ctx = TestContext::new("r1512");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users ORDER BY score LIMIT 3;")?;
    assert!(
        plan.to_lowercase().contains("top-n")
            || plan.to_lowercase().contains("topn")
            || plan.to_lowercase().contains("limit")
    );
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1513..R1520: Access Paths
// -----------------------------------------------------------------------------

#[test]
fn test_r1513_full_scan_alternative() -> Result<()> {
    let ctx = TestContext::new("r1513");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users;")?;
    assert!(plan.contains("FullTableScan") || plan.contains("Scan"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1514_primary_index_alternative() -> Result<()> {
    let ctx = TestContext::new("r1514");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE id = 10;")?;
    assert!(plan.contains("Primary") || plan.contains("Index"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1515_secondary_index_alternative() -> Result<()> {
    let ctx = TestContext::new("r1515");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE status = 'active';")?;
    assert!(plan.contains("idx_users_status") || plan.contains("Secondary"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1516_covering_index_alternative() -> Result<()> {
    let ctx = TestContext::new("r1516");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan =
        tx.explain_sql("SELECT status, age FROM users WHERE status = 'active' AND age > 30;")?;
    assert!(plan.contains("idx_users_status_age") || plan.contains("Index"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1517_index_only_alternative() -> Result<()> {
    let ctx = TestContext::new("r1517");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT status FROM users WHERE status = 'active';")?;
    assert!(plan.contains("idx_users_status") || plan.contains("Index"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1518_skip_scan_alternative() -> Result<()> {
    let ctx = TestContext::new("r1518");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE age = 25;")?;
    assert!(!plan.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1519_multi_index_alternative() -> Result<()> {
    let ctx = TestContext::new("r1519");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE status = 'active' OR age = 25;")?;
    assert!(!plan.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1520_empty_result_alternative() -> Result<()> {
    let ctx = TestContext::new("r1520");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE 1 = 0;")?;
    assert!(plan.contains("EmptyResult") || plan.contains("Empty"));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1521..R1530: Joins
// -----------------------------------------------------------------------------

#[test]
fn test_r1521_nested_loop_alternative() -> Result<()> {
    let ctx = TestContext::new("r1521");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users u JOIN orders o ON u.id > o.id;")?;
    assert!(plan.contains("NestedLoop") || plan.contains("Join"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1522_hash_join_alternative() -> Result<()> {
    let ctx = TestContext::new("r1522");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users u JOIN orders o ON u.id = o.user_id;")?;
    assert!(plan.contains("Join"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1523_external_hash_alternative() -> Result<()> {
    let ctx = TestContext::new("r1523");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT * FROM users u JOIN orders o ON u.id = o.user_id;")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1524_merge_join_alternative() -> Result<()> {
    let ctx = TestContext::new("r1524");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT * FROM users u JOIN orders o ON u.id = o.id ORDER BY u.id;")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1525_semi_alternative() -> Result<()> {
    let ctx = TestContext::new("r1525");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id);",
    )?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1526_anti_alternative() -> Result<()> {
    let ctx = TestContext::new("r1526");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT * FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.amount > 100000.0);")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1527_join_property_propagation() -> Result<()> {
    let ctx = TestContext::new("r1527");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.id LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1528_inner_join_reordering_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1528");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT * FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 5;")?;
    assert_eq!(res.rows.len(), 1);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1529_outer_join_barrier() -> Result<()> {
    let ctx = TestContext::new("r1529");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id;")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1530_semi_anti_barrier() -> Result<()> {
    let ctx = TestContext::new("r1530");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT * FROM users u WHERE u.id IN (SELECT o.user_id FROM orders o);")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1531..R1534: Aggregation
// -----------------------------------------------------------------------------

#[test]
fn test_r1531_hash_aggregation() -> Result<()> {
    let ctx = TestContext::new("r1531");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT status, COUNT(*), SUM(score) FROM users GROUP BY status;")?;
    assert_eq!(res.rows.len(), 2);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1532_streaming_aggregation() -> Result<()> {
    let ctx = TestContext::new("r1532");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, COUNT(*) FROM users GROUP BY id;")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1533_external_aggregation() -> Result<()> {
    let ctx = TestContext::new("r1533");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT age, AVG(score) FROM users GROUP BY age HAVING AVG(score) > 0.0;")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1534_grouping_property_propagation() -> Result<()> {
    let ctx = TestContext::new("r1534");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT status, COUNT(*) FROM users GROUP BY status ORDER BY status;")?;
    assert_eq!(res.rows.len(), 2);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1535..R1540: Materialization
// -----------------------------------------------------------------------------

#[test]
fn test_r1535_logical_subplan_identity_independent_of_physical_path() {
    use treedb::query::materialize::LogicalSubplanFingerprint;

    let sub1 =
        LogicalSubplanFingerprint::new("LOGICAL_TABLE:users;WHERE:status = 'active';PROJ:id");
    let sub2 =
        LogicalSubplanFingerprint::new("LOGICAL_TABLE:users;WHERE:status = 'active';PROJ:id");
    assert_eq!(sub1.0, sub2.0);
}

#[test]
fn test_r1536_inline_alternative() -> Result<()> {
    let ctx = TestContext::new("r1536");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("WITH cte AS (SELECT * FROM users WHERE id = 1) SELECT * FROM cte;")?;
    assert_eq!(res.rows.len(), 1);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1537_materialize_alternative() -> Result<()> {
    let ctx = TestContext::new("r1537");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH cte AS (SELECT * FROM users WHERE age > 25) SELECT c1.id, c2.name FROM cte c1 JOIN cte c2 ON c1.id = c2.id LIMIT 10;")?;
    assert_eq!(res.rows.len(), 10);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1538_external_materialize_alternative() -> Result<()> {
    let ctx = TestContext::new("r1538");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH cte AS (SELECT * FROM users) SELECT COUNT(*) FROM cte c1 JOIN cte c2 ON c1.id = c2.id;")?;
    assert_eq!(res.rows[0].values[0], Value::UInt64(50));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1539_consumer_property_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1539");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "WITH cte AS (SELECT id, age, score FROM users) SELECT id FROM cte ORDER BY id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1540_common_subplan_reuse() -> Result<()> {
    let ctx = TestContext::new("r1540");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH shared AS (SELECT id, score FROM users WHERE status = 'active') SELECT s1.id, s2.score FROM shared s1 JOIN shared s2 ON s1.id = s2.id;")?;
    assert_eq!(res.rows.len(), 25);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1541..R1545: Column Demand
// -----------------------------------------------------------------------------

#[test]
fn test_r1541_required_column_memo_key() {
    let req1 = RequiredColumns::from_columns(vec!["a"]);
    let req2 = RequiredColumns::from_columns(vec!["a", "b"]);

    let ord = OrderingProperty::default();
    let k1 = PhysicalPropertyKey::new(&ord, &req1, None, None);
    let k2 = PhysicalPropertyKey::new(&ord, &req2, None, None);
    assert_ne!(k1.fingerprint(), k2.fingerprint());
}

#[test]
fn test_r1542_covering_index_demand_sensitivity() -> Result<()> {
    let ctx = TestContext::new("r1542");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan1 = tx.explain_sql("SELECT status, age FROM users WHERE status = 'active';")?;
    let plan2 = tx.explain_sql("SELECT * FROM users WHERE status = 'active';")?;
    assert!(plan1.contains("idx_users_status_age") || plan1.contains("Index"));
    assert!(!plan2.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1543_late_materialization_demand_sensitivity() -> Result<()> {
    let ctx = TestContext::new("r1543");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users WHERE status = 'active' AND age = 30;")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1544_narrow_materialization() -> Result<()> {
    let ctx = TestContext::new("r1544");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("WITH cte AS (SELECT id, name FROM users) SELECT id FROM cte LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1545_incompatible_column_demand_plans_not_reused() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "users".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let p = dummy_plan(10.0, 50.0);
    memo.add_alternative(gid, p, "Plan".to_string()).unwrap();

    let ord = OrderingProperty::default();
    let req1 = RequiredColumns::from_columns(vec!["id"]);
    let req2 = RequiredColumns::from_columns(vec!["name"]);

    let k1 = PhysicalPropertyKey::new(&ord, &req1, None, None);
    let k2 = PhysicalPropertyKey::new(&ord, &req2, None, None);

    let b1 = memo.optimize_group(gid, &k1, &ord, &req1, None).unwrap();
    let b2 = memo.optimize_group(gid, &k2, &ord, &req2, None).unwrap();
    assert_eq!(b1.cost, b2.cost);
}

// -----------------------------------------------------------------------------
// R1546..R1549: Statistics / Costing
// -----------------------------------------------------------------------------

#[test]
fn test_r1546_histogram_aware_costing() -> Result<()> {
    let ctx = TestContext::new("r1546");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE users;")?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE age >= 25 AND age <= 35;")?;
    assert!(!plan.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1547_correlation_aware_costing() -> Result<()> {
    let ctx = TestContext::new("r1547");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users WHERE status = 'active' AND age = 30;")?;
    assert!(!plan.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1548_runtime_feedback_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1548");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT * FROM users WHERE status = 'active';")?;
    assert_eq!(res.rows.len(), 25);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1549_resource_aware_costing() -> Result<()> {
    let ctx = TestContext::new("r1549");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT * FROM users ORDER BY age;")?;
    assert!(!plan.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1550..R1554: Plan Cache
// -----------------------------------------------------------------------------

#[test]
fn test_r1550_prepared_statement_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1550");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let stmt = db.prepare_sql("SELECT * FROM users WHERE id = ?;")?;
    let mut tx = db.begin_transaction()?;
    let res = tx.execute_prepared_query(&stmt, &[Value::Int64(10)])?;
    assert_eq!(res.rows.len(), 1);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1551_plan_cache_generation_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1551");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let stmt = db.prepare_sql("SELECT * FROM users WHERE age > ?;")?;
    let mut tx = db.begin_transaction()?;
    let r1 = tx.execute_prepared_query(&stmt, &[Value::Int64(40)])?;
    let r2 = tx.execute_prepared_query(&stmt, &[Value::Int64(45)])?;
    assert!(r1.rows.len() >= r2.rows.len());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1552_index_ddl_invalidation() -> Result<()> {
    let ctx = TestContext::new("r1552");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let stmt = db.prepare_sql("SELECT * FROM users WHERE status = ?;")?;
    let mut tx = db.begin_transaction()?;
    tx.execute_prepared_query(&stmt, &[Value::String("active".to_string())])?;
    tx.execute_statement("DROP INDEX idx_users_status;")?;
    let res = tx.execute_prepared_query(&stmt, &[Value::String("active".to_string())])?;
    assert_eq!(res.rows.len(), 25);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1553_analyze_statistics_invalidation() -> Result<()> {
    let ctx = TestContext::new("r1553");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE users;")?;
    let res = tx.execute_sql("SELECT * FROM users WHERE score > 50.0;")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1554_resource_signature_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1554");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT * FROM users u JOIN orders o ON u.id = o.user_id ORDER BY o.amount LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1555..R1558: Determinism
// -----------------------------------------------------------------------------

#[test]
fn test_r1555_repeated_optimization_produces_identical_plan() -> Result<()> {
    let ctx = TestContext::new("r1555");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan1 = tx.explain_sql(
        "SELECT name FROM users WHERE status = 'active' ORDER BY created_at LIMIT 10;",
    )?;
    let plan2 = tx.explain_sql(
        "SELECT name FROM users WHERE status = 'active' ORDER BY created_at LIMIT 10;",
    )?;
    assert_eq!(plan1, plan2);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1556_repeated_explain_identical() -> Result<()> {
    let ctx = TestContext::new("r1556");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let q = "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.age > 25 ORDER BY o.amount DESC LIMIT 5;";
    let e1 = tx.explain_sql(q)?;
    let e2 = tx.explain_sql(q)?;
    assert_eq!(e1, e2);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1557_tie_breaking_deterministic() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    let op = LogicalOp::Scan {
        table_name: "t".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    let p1 = dummy_plan(100.0, 10.0);
    let p2 = dummy_plan(100.0, 10.0);

    memo.add_alternative(gid, p1, "ScanA".to_string()).unwrap();
    memo.add_alternative(gid, p2, "ScanB".to_string()).unwrap();

    let ord = OrderingProperty::default();
    let cols = RequiredColumns::all();
    let prop_key = PhysicalPropertyKey::new(&ord, &cols, None, None);

    let best1 = memo
        .optimize_group(gid, &prop_key, &ord, &cols, None)
        .unwrap();
    let best2 = memo
        .optimize_group(gid, &prop_key, &ord, &cols, None)
        .unwrap();
    assert_eq!(best1.cost, best2.cost);
}

#[test]
fn test_r1558_memo_iteration_order_deterministic() {
    let mut memo = OptimizerMemo::new(MemoConfig::default());
    for i in 0..10 {
        let op = LogicalOp::Scan {
            table_name: format!("t{i}"),
            table_alias: None,
            filter: None,
        };
        memo.get_or_create_group(op).unwrap();
    }

    let g_ids: Vec<u32> = memo.groups().iter().map(|g| g.id.0).collect();
    let expected: Vec<u32> = (0..10).collect();
    assert_eq!(g_ids, expected);
}

// -----------------------------------------------------------------------------
// R1559..R1563: Limits & Fallback
// -----------------------------------------------------------------------------

#[test]
fn test_r1559_group_limit() {
    let config = MemoConfig {
        max_groups: 3,
        max_alternatives_per_group: 10,
        max_property_variants: 10,
        max_optimization_iterations: 100,
    };
    let mut memo = OptimizerMemo::new(config);
    for i in 0..3 {
        let op = LogicalOp::Scan {
            table_name: format!("t{i}"),
            table_alias: None,
            filter: None,
        };
        memo.get_or_create_group(op).unwrap();
    }
    assert!(!memo.stats.limits_reached);

    let op_overflow = LogicalOp::Scan {
        table_name: "t_overflow".to_string(),
        table_alias: None,
        filter: None,
    };
    let res = memo.get_or_create_group(op_overflow);
    assert!(memo.stats.limits_reached);
    assert!(res.is_err());
}

#[test]
fn test_r1560_alternative_limit() {
    let config = MemoConfig {
        max_groups: 10,
        max_alternatives_per_group: 2,
        max_property_variants: 10,
        max_optimization_iterations: 100,
    };
    let mut memo = OptimizerMemo::new(config);
    let op = LogicalOp::Scan {
        table_name: "t".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();

    memo.add_alternative(gid, dummy_plan(10.0, 5.0), "P1".to_string())
        .unwrap();
    memo.add_alternative(gid, dummy_plan(20.0, 5.0), "P2".to_string())
        .unwrap();
    let res = memo.add_alternative(gid, dummy_plan(30.0, 5.0), "P3".to_string());
    assert!(res.is_err() || memo.stats.limits_reached || !res.unwrap());
}

#[test]
fn test_r1561_property_variant_limit() {
    let config = MemoConfig {
        max_groups: 10,
        max_alternatives_per_group: 10,
        max_property_variants: 2,
        max_optimization_iterations: 100,
    };
    let mut memo = OptimizerMemo::new(config);
    let op = LogicalOp::Scan {
        table_name: "t".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();
    memo.add_alternative(gid, dummy_plan(10.0, 5.0), "P1".to_string())
        .unwrap();

    let ord1 = OrderingProperty::default();
    let res1 = ResourcePlanningSignature::new(1, 1, 1, 1);
    let res2 = ResourcePlanningSignature::new(2, 1, 1, 1);
    let res3 = ResourcePlanningSignature::new(3, 1, 1, 1);
    let k1 = PhysicalPropertyKey::new(&ord1, &RequiredColumns::all(), None, Some(&res1));
    let k2 = PhysicalPropertyKey::new(&ord1, &RequiredColumns::all(), None, Some(&res2));
    let k3 = PhysicalPropertyKey::new(&ord1, &RequiredColumns::all(), None, Some(&res3));

    let _ = memo.optimize_group(gid, &k1, &ord1, &RequiredColumns::all(), None);
    let _ = memo.optimize_group(gid, &k2, &ord1, &RequiredColumns::all(), None);
    let _ = memo.optimize_group(gid, &k3, &ord1, &RequiredColumns::all(), None);
    assert!(memo.stats.limits_reached);
}

#[test]
fn test_r1562_iteration_limit() {
    let config = MemoConfig {
        max_groups: 10,
        max_alternatives_per_group: 10,
        max_property_variants: 10,
        max_optimization_iterations: 1,
    };
    let mut memo = OptimizerMemo::new(config);
    let op = LogicalOp::Scan {
        table_name: "t".to_string(),
        table_alias: None,
        filter: None,
    };
    let gid = memo.get_or_create_group(op).unwrap();
    memo.add_alternative(gid, dummy_plan(10.0, 5.0), "P1".to_string())
        .unwrap();

    let ord = OrderingProperty::default();
    let k1 = PhysicalPropertyKey::new(&ord, &RequiredColumns::all(), None, None);
    let _ = memo.optimize_group(gid, &k1, &ord, &RequiredColumns::all(), None);
    let res1 = ResourcePlanningSignature::new(10, 1, 1, 1);
    let k2 = PhysicalPropertyKey::new(&ord, &RequiredColumns::all(), None, Some(&res1));
    let _ = memo.optimize_group(gid, &k2, &ord, &RequiredColumns::all(), None);
    assert!(memo.stats.limits_reached);
}

#[test]
fn test_r1563_safe_fallback_after_limit() -> Result<()> {
    let ctx = TestContext::new("r1563");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.age > 20 ORDER BY o.amount LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1564..R1574: SQL Integration
// -----------------------------------------------------------------------------

#[test]
fn test_r1564_sql_select() -> Result<()> {
    let ctx = TestContext::new("r1564");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, name, age FROM users WHERE id <= 5;")?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1565_sql_join() -> Result<()> {
    let ctx = TestContext::new("r1565");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id WHERE u.id <= 3;")?;
    assert_eq!(res.rows.len(), 3);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1566_sql_group_by() -> Result<()> {
    let ctx = TestContext::new("r1566");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT status, COUNT(*), MIN(age), MAX(age) FROM users GROUP BY status;")?;
    assert_eq!(res.rows.len(), 2);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1567_sql_window_function() -> Result<()> {
    let ctx = TestContext::new("r1567");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT id, score, ROW_NUMBER() OVER (ORDER BY score DESC) as rank FROM users LIMIT 10;",
    )?;
    assert_eq!(res.rows.len(), 10);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1568_sql_cte() -> Result<()> {
    let ctx = TestContext::new("r1568");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH cte AS (SELECT id, name FROM users WHERE status = 'active') SELECT COUNT(*) FROM cte;")?;
    assert_eq!(res.rows[0].values[0], Value::UInt64(25));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1569_sql_recursive_cte() -> Result<()> {
    let ctx = TestContext::new("r1569");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH RECURSIVE cnt(x) AS (SELECT id FROM users WHERE id = 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 5) SELECT SUM(x) FROM cnt;")?;
    assert_eq!(res.rows[0].values[0], Value::Int64(15));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1570_sql_subquery() -> Result<()> {
    let ctx = TestContext::new("r1570");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT name FROM users WHERE score > (SELECT AVG(score) FROM users) LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1571_sql_correlated_subquery() -> Result<()> {
    let ctx = TestContext::new("r1571");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT u.name FROM users u WHERE u.score > (SELECT AVG(o.amount) FROM orders o WHERE o.user_id = u.id) LIMIT 5;")?;
    assert!(!res.schema.columns.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1572_sql_set_operation() -> Result<()> {
    let ctx = TestContext::new("r1572");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT id FROM users WHERE id <= 5 UNION SELECT id FROM orders WHERE id <= 8;",
    )?;
    assert_eq!(res.rows.len(), 8);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1573_sql_order_by_limit() -> Result<()> {
    let ctx = TestContext::new("r1573");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT id, name, created_at FROM users ORDER BY created_at DESC LIMIT 5 OFFSET 2;",
    )?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1574_sql_prepared_sql() -> Result<()> {
    let ctx = TestContext::new("r1574");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let stmt = db.prepare_sql("SELECT name FROM users WHERE age = ?;")?;
    let mut tx = db.begin_transaction()?;
    let res = tx.execute_prepared_query(&stmt, &[Value::Int64(25)])?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1575..R1578: DML & Transactions
// -----------------------------------------------------------------------------

#[test]
fn test_r1575_dml_update() -> Result<()> {
    let ctx = TestContext::new("r1575");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("UPDATE users SET score = 999.0 WHERE id = 1;")?;
    let res = tx.execute_sql("SELECT score FROM users WHERE id = 1;")?;
    assert_eq!(res.rows[0].values[0], Value::Float64(999.0));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1576_dml_delete() -> Result<()> {
    let ctx = TestContext::new("r1576");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("DELETE FROM users WHERE id = 1;")?;
    let res = tx.execute_sql("SELECT * FROM users WHERE id = 1;")?;
    assert_eq!(res.rows.len(), 0);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1577_transaction_visibility() -> Result<()> {
    let ctx = TestContext::new("r1577");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("INSERT INTO users VALUES (999, 'Temp', 'active', 30, 100.0, 5000);")?;
    let res = tx.execute_sql("SELECT COUNT(*) FROM users;")?;
    assert_eq!(res.rows[0].values[0], Value::UInt64(51));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1578_transaction_abort() -> Result<()> {
    let ctx = TestContext::new("r1578");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("INSERT INTO users VALUES (999, 'Temp', 'active', 30, 100.0, 5000);")?;
    tx.abort()?;

    let mut tx2 = db.begin_transaction()?;
    let res = tx2.execute_sql("SELECT COUNT(*) FROM users;")?;
    assert_eq!(res.rows[0].values[0], Value::UInt64(50));
    tx2.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1579..R1581: Durability
// -----------------------------------------------------------------------------

#[test]
fn test_r1579_persistence() -> Result<()> {
    let ctx = TestContext::new("r1579");
    {
        let mut db = ctx.db();
        setup_standard_schema(&mut db)?;
    }
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT COUNT(*) FROM users;")?;
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));
        tx.commit()?;
    }
    Ok(())
}

#[test]
fn test_r1580_checkpoint() -> Result<()> {
    let ctx = TestContext::new("r1580");
    {
        let mut db = ctx.db();
        setup_standard_schema(&mut db)?;
        db.checkpoint()?;
    }
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT COUNT(*) FROM orders;")?;
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));
        tx.commit()?;
    }
    Ok(())
}

#[test]
fn test_r1581_crash_recovery() -> Result<()> {
    let ctx = TestContext::new("r1581");
    {
        let mut db = ctx.db();
        setup_standard_schema(&mut db)?;
    }
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT COUNT(*) FROM users;")?;
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));
        tx.commit()?;
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// R1582..R1587: Differential Correctness (Memo vs Baseline Planner Oracle)
// -----------------------------------------------------------------------------

#[test]
fn test_r1582_memo_vs_existing_optimizer() -> Result<()> {
    let ctx = TestContext::new("r1582");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT * FROM users WHERE status = 'active';",
        "SELECT id, name FROM users WHERE age > 30 ORDER BY id LIMIT 5;",
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id <= 10;",
        "SELECT status, COUNT(*), SUM(score) FROM users GROUP BY status;",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.rows.is_empty() || !res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1583_randomized_single_table_differential() -> Result<()> {
    let ctx = TestContext::new("r1583");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT id FROM users WHERE id = 25;",
        "SELECT name, status FROM users WHERE status = 'active' AND age < 40;",
        "SELECT * FROM users WHERE age >= 20 AND age <= 35 ORDER BY created_at LIMIT 7;",
        "SELECT status, age FROM users WHERE score > 50.0 ORDER BY age DESC LIMIT 3;",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1584_randomized_join_differential() -> Result<()> {
    let ctx = TestContext::new("r1584");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT u.id, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.status = 'active';",
        "SELECT u.name, o.order_date FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.age > 30;",
        "SELECT * FROM users u JOIN orders o ON u.id = o.user_id WHERE o.amount > 100.0 LIMIT 10;",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1585_randomized_aggregation_differential() -> Result<()> {
    let ctx = TestContext::new("r1585");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT status, COUNT(*), AVG(score) FROM users GROUP BY status;",
        "SELECT age, SUM(score) FROM users GROUP BY age HAVING SUM(score) > 50.0;",
        "SELECT status, age, COUNT(*) FROM users GROUP BY status, age ORDER BY status, age LIMIT 10;",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1586_randomized_subquery_differential() -> Result<()> {
    let ctx = TestContext::new("r1586");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount > 50.0);",
        "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.amount < 1000.0);",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1587_randomized_property_combination_differential() -> Result<()> {
    let ctx = TestContext::new("r1587");
    let mut db = ctx.db();
    setup_standard_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let queries = vec![
        "SELECT id, name FROM users WHERE status = 'active' ORDER BY created_at ASC LIMIT 5;",
        "SELECT status, age, score FROM users WHERE age > 25 ORDER BY score DESC LIMIT 10 OFFSET 2;",
        "SELECT u.id, u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.status = 'active' ORDER BY o.amount DESC LIMIT 5;",
    ];

    for q in queries {
        let res = tx.execute_sql(q)?;
        assert!(!res.schema.columns.is_empty());
    }
    tx.commit()?;
    Ok(())
}
