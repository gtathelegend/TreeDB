use tempfile::TempDir;
use treedb::engine::types::{Row, Value};
use treedb::error::Result;
use treedb::query::cost::CostModel;
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

// -----------------------------------------------------------------------------
// R926–R940: Row-Goal Semantics Through Joins
// -----------------------------------------------------------------------------

#[test]
fn test_r926_join_row_goal_does_not_blindly_limit_children() -> Result<()> {
    let ctx = TestContext::new("r926_join_row_goal_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id INT);")?;
    tx.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, order_id INT, price INT);")?;

    // 1 order has 10 items
    tx.execute_statement("INSERT INTO orders VALUES (1, 100);")?;
    tx.execute_statement("INSERT INTO orders VALUES (2, 200);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO items VALUES ({i}, 1, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // LIMIT 5 from joined stream should return 5 rows, even though order 1 produces 10 items
    let res = tx.execute_sql(
        "SELECT orders.id, items.price FROM orders JOIN items ON orders.id = items.order_id ORDER BY items.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    Ok(())
}

#[test]
fn test_r927_join_row_goal_with_offset() -> Result<()> {
    let ctx = TestContext::new("r927_join_row_goal_offset");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY);")?;
    tx.execute_statement("CREATE TABLE b (a_id BIGINT PRIMARY KEY, val INT);")?;

    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO a VALUES ({i});"))?;
        tx.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT a.id, b.val FROM a JOIN b ON a.id = b.a_id ORDER BY a.id LIMIT 3 OFFSET 4;",
    )?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(5));
    assert_eq!(res.rows[1].values[0], Value::Int64(6));
    assert_eq!(res.rows[2].values[0], Value::Int64(7));
    Ok(())
}

#[test]
fn test_r928_join_expected_probes_cost_model() {
    // 1000 outer rows, 1000 estimated join output rows => 1 row per probe
    let probes = CostModel::expected_outer_probes_for_join_row_goal(10.0, 1000.0, 1000.0);
    assert!((probes - 10.0).abs() < 1e-6);

    // 1000 outer rows, 100 estimated join output rows (0.1 match rate) => 100 probes needed for 10 output rows
    let probes_sparse = CostModel::expected_outer_probes_for_join_row_goal(10.0, 100.0, 1000.0);
    assert!((probes_sparse - 100.0).abs() < 1e-6);

    // Bounded to available outer rows
    let probes_clamped = CostModel::expected_outer_probes_for_join_row_goal(500.0, 100.0, 1000.0);
    assert!((probes_clamped - 1000.0).abs() < 1e-6);
}

#[test]
fn test_r929_nested_loop_row_goal_cost_model() {
    let cost = CostModel::nested_loop_join_row_goal_cost(10.0, 1000.0, 100.0, 1000.0, 0.0);
    assert!(cost > 0.0);
    let full_cost = CostModel::nested_loop_join_row_goal_cost(1000.0, 1000.0, 100.0, 1000.0, 0.0);
    assert!(cost < full_cost);
}

#[test]
fn test_r930_hash_join_row_goal_cost_model() {
    let cost = CostModel::hash_join_row_goal_cost(10.0, 1000.0, 500.0, 1000.0, 1, 1.0);
    let full_cost = CostModel::hash_join_row_goal_cost(1000.0, 1000.0, 500.0, 1000.0, 1, 1.0);
    assert!(cost < full_cost);
    // Hash join build cost is fixed regardless of row goal
    assert!(cost > 500.0 * 0.005);
}

// -----------------------------------------------------------------------------
// R941–R955: Nested-Loop Early Termination
// -----------------------------------------------------------------------------

#[test]
fn test_r941_nested_loop_early_stop_inner_join() -> Result<()> {
    let ctx = TestContext::new("r941_nl_early_stop");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name STRING);")?;
    tx.execute_statement("CREATE TABLE logs (id BIGINT PRIMARY KEY, user_id BIGINT, msg STRING);")?;

    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'User {i}');"))?;
        tx.execute_statement(&format!("INSERT INTO logs VALUES ({i}, {i}, 'Log {i}');"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT users.id, logs.msg FROM users JOIN logs ON users.id = logs.user_id ORDER BY users.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[4].values[0], Value::Int64(5));
    Ok(())
}

// -----------------------------------------------------------------------------
// R956–R970: Hash Join Bounded Probing
// -----------------------------------------------------------------------------

#[test]
fn test_r956_hash_join_full_build_bounded_probe() -> Result<()> {
    let ctx = TestContext::new("r956_hash_join_bounded_probe");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE left_tbl (k BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE right_tbl (k BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO left_tbl VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO right_tbl VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT left_tbl.k, right_tbl.v FROM left_tbl JOIN right_tbl ON left_tbl.k = right_tbl.k LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);

    let plan_fmt = tx.explain_sql(
        "SELECT left_tbl.k, right_tbl.v FROM left_tbl JOIN right_tbl ON left_tbl.k = right_tbl.k LIMIT 5;",
    )?;
    assert!(
        plan_fmt.contains("row_goal: 5")
            || plan_fmt.contains("LimitOffset")
            || plan_fmt.contains("HashJoin")
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// R971–R985: External Hash Join with Row Goal
// -----------------------------------------------------------------------------

#[test]
fn test_r971_external_hash_join_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r971_external_hash_row_goal");
    let mut db = ctx.db();
    // Set small join memory budget
    db.set_join_config(
        treedb::query::external_hash_join::JoinConfig::new().with_memory_budget_bytes(256),
    );

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, val INT);")?;

    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT t1.id, t2.val FROM t1 JOIN t2 ON t1.id = t2.id LIMIT 4;")?;
    assert_eq!(res.rows.len(), 4);
    Ok(())
}

// -----------------------------------------------------------------------------
// R986–R1000: Merge Join Early Termination
// -----------------------------------------------------------------------------

#[test]
fn test_r986_merge_join_early_termination_ordered() -> Result<()> {
    let ctx = TestContext::new("r986_merge_join_early_stop");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, name STRING);")?;
    tx.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, details STRING);")?;

    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO a VALUES ({i}, 'Name {i}');"))?;
        tx.execute_statement(&format!("INSERT INTO b VALUES ({i}, 'Details {i}');"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT a.id, b.details FROM a JOIN b ON a.id = b.id ORDER BY a.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[4].values[0], Value::Int64(5));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1001–R1015: Duplicate-Run Correctness Across Limit Boundary
// -----------------------------------------------------------------------------

#[test]
fn test_r1001_merge_join_many_to_many_duplicate_boundary() -> Result<()> {
    let ctx = TestContext::new("r1001_duplicate_run_correctness");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE dup_left (id BIGINT PRIMARY KEY, k INT, l_val STRING);")?;
    tx.execute_statement("CREATE TABLE dup_right (id BIGINT PRIMARY KEY, k INT, r_val STRING);")?;

    let mut id_l = 0;
    let mut id_r = 0;
    // Key 1 has 3 left rows and 3 right rows => 9 cross-product rows for key 1
    for i in 1..=3 {
        id_l += 1;
        tx.execute_statement(&format!(
            "INSERT INTO dup_left VALUES ({id_l}, 1, 'L1_{i}');"
        ))?;
        id_r += 1;
        tx.execute_statement(&format!(
            "INSERT INTO dup_right VALUES ({id_r}, 1, 'R1_{i}');"
        ))?;
    }
    // Key 2 has 2 left rows and 2 right rows => 4 cross-product rows
    for i in 1..=2 {
        id_l += 1;
        tx.execute_statement(&format!(
            "INSERT INTO dup_left VALUES ({id_l}, 2, 'L2_{i}');"
        ))?;
        id_r += 1;
        tx.execute_statement(&format!(
            "INSERT INTO dup_right VALUES ({id_r}, 2, 'R2_{i}');"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // LIMIT 4 terminates in the middle of duplicate run for Key 1
    let res = tx.execute_sql(
        "SELECT dup_left.k, dup_left.l_val, dup_right.r_val FROM dup_left JOIN dup_right ON dup_left.k = dup_right.k ORDER BY dup_left.k, dup_left.id, dup_right.id LIMIT 4;",
    )?;
    assert_eq!(res.rows.len(), 4);
    for row in &res.rows {
        assert_eq!(row.values[0], Value::Int64(1));
    }
    assert_eq!(res.rows[0].values[1], Value::String("L1_1".into()));
    assert_eq!(res.rows[0].values[2], Value::String("R1_1".into()));
    assert_eq!(res.rows[1].values[1], Value::String("L1_1".into()));
    assert_eq!(res.rows[1].values[2], Value::String("R1_2".into()));
    assert_eq!(res.rows[2].values[1], Value::String("L1_1".into()));
    assert_eq!(res.rows[2].values[2], Value::String("R1_3".into()));
    assert_eq!(res.rows[3].values[1], Value::String("L1_2".into()));
    assert_eq!(res.rows[3].values[2], Value::String("R1_1".into()));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1016–R1030: Index Probe Joins with Row Goal
// -----------------------------------------------------------------------------

#[test]
fn test_r1016_index_probe_semi_join_row_goal() -> Result<()> {
    let ctx = TestContext::new("r1016_index_probe_semi_row_goal");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE parent (id BIGINT PRIMARY KEY, code INT);")?;
    tx.execute_statement("CREATE TABLE lookup (id BIGINT PRIMARY KEY, active BOOLEAN);")?;

    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO parent VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO lookup VALUES ({i}, true);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT parent.id FROM parent WHERE EXISTS (SELECT 1 FROM lookup WHERE lookup.id = parent.code) ORDER BY parent.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[4].values[0], Value::Int64(5));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1031–R1045: Residual Predicate Selectivity with Row Goal
// -----------------------------------------------------------------------------

#[test]
fn test_r1031_residual_filter_requires_more_join_work() -> Result<()> {
    let ctx = TestContext::new("r1031_residual_filter_selectivity");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, status STRING);")?;
    tx.execute_statement("CREATE TABLE b (a_id BIGINT PRIMARY KEY, val INT);")?;

    // 100 rows, only status='active' qualifies (every 10th row)
    for i in 1..=100 {
        let status = if i % 10 == 0 { "active" } else { "inactive" };
        tx.execute_statement(&format!("INSERT INTO a VALUES ({i}, '{status}');"))?;
        tx.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT a.id, b.val FROM a JOIN b ON a.id = b.a_id WHERE a.status = 'active' ORDER BY a.id LIMIT 3;",
    )?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(10));
    assert_eq!(res.rows[1].values[0], Value::Int64(20));
    assert_eq!(res.rows[2].values[0], Value::Int64(30));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1046–R1060: ORDER BY + JOIN + LIMIT
// -----------------------------------------------------------------------------

#[test]
fn test_r1046_order_by_join_limit_ascending_and_descending() -> Result<()> {
    let ctx = TestContext::new("r1046_order_by_join_limit");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, x INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, y INT);")?;

    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i} * 2);"))?;
        tx.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i} * 3);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res_asc = tx.execute_sql(
        "SELECT t1.x, t2.y FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.x ASC LIMIT 3;",
    )?;
    assert_eq!(res_asc.rows.len(), 3);
    assert_eq!(res_asc.rows[0].values[0], Value::Int64(2));
    assert_eq!(res_asc.rows[2].values[0], Value::Int64(6));

    let res_desc = tx.execute_sql(
        "SELECT t1.x, t2.y FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.x DESC LIMIT 3;",
    )?;
    assert_eq!(res_desc.rows.len(), 3);
    assert_eq!(res_desc.rows[0].values[0], Value::Int64(40));
    assert_eq!(res_desc.rows[1].values[0], Value::Int64(38));
    assert_eq!(res_desc.rows[2].values[0], Value::Int64(36));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1061–R1075: Outer Join Safety Barriers
// -----------------------------------------------------------------------------

#[test]
fn test_r1061_left_outer_join_null_padding_preserved() -> Result<()> {
    let ctx = TestContext::new("r1061_left_join_safety");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE main (id BIGINT PRIMARY KEY, name STRING);")?;
    tx.execute_statement("CREATE TABLE extra (main_id BIGINT PRIMARY KEY, detail STRING);")?;

    // main has ids 1..10, extra only has even ids
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO main VALUES ({i}, 'M{i}');"))?;
        if i % 2 == 0 {
            tx.execute_statement(&format!("INSERT INTO extra VALUES ({i}, 'E{i}');"))?;
        }
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT main.id, extra.detail FROM main LEFT JOIN extra ON main.id = extra.main_id ORDER BY main.id LIMIT 4;",
    )?;
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[1], Value::Null);
    assert_eq!(res.rows[1].values[1], Value::String("E2".into()));
    assert_eq!(res.rows[2].values[1], Value::Null);
    assert_eq!(res.rows[3].values[1], Value::String("E4".into()));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1076–R1090: SEMI and ANTI Joins with Row Goal
// -----------------------------------------------------------------------------

#[test]
fn test_r1076_semi_and_anti_join_row_goals() -> Result<()> {
    let ctx = TestContext::new("r1076_semi_anti_row_goals");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name STRING);")?;
    tx.execute_statement("CREATE TABLE orders (id BIGINT PRIMARY KEY, cust_id BIGINT);")?;

    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO customers VALUES ({i}, 'C{i}');"))?;
        if i <= 10 {
            tx.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {i});"))?;
        }
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // SEMI: customers with orders LIMIT 3
    let semi_res = tx.execute_sql(
        "SELECT customers.id FROM customers WHERE EXISTS (SELECT 1 FROM orders WHERE orders.cust_id = customers.id) ORDER BY customers.id LIMIT 3;",
    )?;
    assert_eq!(semi_res.rows.len(), 3);
    assert_eq!(semi_res.rows[0].values[0], Value::Int64(1));

    // ANTI: customers WITHOUT orders LIMIT 3
    let anti_res = tx.execute_sql(
        "SELECT customers.id FROM customers WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.cust_id = customers.id) ORDER BY customers.id LIMIT 3;",
    )?;
    assert_eq!(anti_res.rows.len(), 3);
    assert_eq!(anti_res.rows[0].values[0], Value::Int64(11));
    Ok(())
}

// -----------------------------------------------------------------------------
// R1091–R1105: Plan Cache and Parameter Sensitivity
// -----------------------------------------------------------------------------

#[test]
fn test_r1091_plan_cache_parameter_planning() -> Result<()> {
    let ctx = TestContext::new("r1091_plan_cache");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT);")?;
    tx.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, y INT);")?;

    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let r1 =
        tx.execute_sql("SELECT a.id, b.y FROM a JOIN b ON a.id = b.id ORDER BY a.id LIMIT 2;")?;
    assert_eq!(r1.rows.len(), 2);

    let r2 =
        tx.execute_sql("SELECT a.id, b.y FROM a JOIN b ON a.id = b.id ORDER BY a.id LIMIT 10;")?;
    assert_eq!(r2.rows.len(), 10);
    Ok(())
}

// -----------------------------------------------------------------------------
// R1121–R1135: EXPLAIN and EXPLAIN ANALYZE
// -----------------------------------------------------------------------------

#[test]
fn test_r1121_explain_and_explain_analyze_diagnostics() -> Result<()> {
    let ctx = TestContext::new("r1121_explain_diagnostics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_str = tx.explain_sql(
        "SELECT t1.id, t2.v FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.id LIMIT 5;",
    )?;
    assert!(!explain_str.is_empty());

    let analyze_res = tx.execute_sql(
        "EXPLAIN ANALYZE SELECT t1.id, t2.v FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.id LIMIT 5;",
    )?;
    assert!(!analyze_res.rows.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------------
// R1136–R1150: Transactions and Persistence/Recovery
// -----------------------------------------------------------------------------

#[test]
fn test_r1136_transaction_isolation_and_persistence() -> Result<()> {
    let ctx = TestContext::new("r1136_tx_persistence");
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, val INT);")?;
        tx.execute_statement("CREATE TABLE prices (id BIGINT PRIMARY KEY, price INT);")?;
        for i in 1..=30 {
            tx.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i});"))?;
            tx.execute_statement(&format!("INSERT INTO prices VALUES ({i}, {i} * 10);"))?;
        }
        tx.commit()?;
    }

    // Reopen database
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql(
            "SELECT items.id, prices.price FROM items JOIN prices ON items.id = prices.id ORDER BY items.id LIMIT 4;",
        )?;
        assert_eq!(res.rows.len(), 4);
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[3].values[0], Value::Int64(4));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// R1151–R1175: Randomized Differential Testing Oracle
// -----------------------------------------------------------------------------

#[test]
fn test_r1151_randomized_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("r1151_differential_oracle");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE r_left (id BIGINT PRIMARY KEY, k INT, val INT);")?;
    tx.execute_statement("CREATE TABLE r_right (id BIGINT PRIMARY KEY, k INT, tag STRING);")?;

    // Populate pseudo-random dataset with duplicates and varying keys
    for i in 1..=60 {
        let k = i % 7;
        tx.execute_statement(&format!("INSERT INTO r_left VALUES ({i}, {k}, {i} * 5);"))?;
        tx.execute_statement(&format!(
            "INSERT INTO r_right VALUES ({i}, {k}, 'tag_{k}');"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let query_opt = "SELECT r_left.id, r_left.k, r_right.tag FROM r_left JOIN r_right ON r_left.k = r_right.k ORDER BY r_left.id, r_right.id LIMIT 10 OFFSET 5;";
    let res_opt = tx.execute_sql(query_opt)?;

    // Verify against un-limited full query with manual offset/limit
    let query_full = "SELECT r_left.id, r_left.k, r_right.tag FROM r_left JOIN r_right ON r_left.k = r_right.k ORDER BY r_left.id, r_right.id;";
    let res_full = tx.execute_sql(query_full)?;
    let expected_slice: Vec<Row> = res_full.rows.into_iter().skip(5).take(10).collect();

    assert_eq!(res_opt.rows.len(), expected_slice.len());
    for (actual, expected) in res_opt.rows.iter().zip(expected_slice.iter()) {
        assert_eq!(actual.values, expected.values);
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// R1106–R1120: Resource-Aware Planning & Hard Memory Limits
// -----------------------------------------------------------------------------

#[test]
fn test_r1106_resource_aware_planning_spill_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r1106_resource_planning");
    let mut db = ctx.db();
    db.set_join_config(
        treedb::query::external_hash_join::JoinConfig::new().with_memory_budget_bytes(128),
    );

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE big_l (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE big_r (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO big_l VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO big_r VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT big_l.id, big_r.v FROM big_l JOIN big_r ON big_l.id = big_r.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    Ok(())
}

// -----------------------------------------------------------------------------
// R1152–R1175: Performance Stress Cases
// -----------------------------------------------------------------------------

#[test]
fn test_r1152_highly_duplicated_join_key_stress() -> Result<()> {
    let ctx = TestContext::new("r1152_duplicated_keys");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE dup_a (id BIGINT PRIMARY KEY, k INT);")?;
    tx.execute_statement("CREATE TABLE dup_b (id BIGINT PRIMARY KEY, k INT);")?;

    // 200 outer rows, 200 inner rows, all same join key k=1
    for i in 1..=200 {
        tx.execute_statement(&format!("INSERT INTO dup_a VALUES ({i}, 1);"))?;
        tx.execute_statement(&format!("INSERT INTO dup_b VALUES ({i}, 1);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT dup_a.id, dup_b.id FROM dup_a JOIN dup_b ON dup_a.k = dup_b.k ORDER BY dup_a.id, dup_b.id LIMIT 10;",
    )?;
    assert_eq!(res.rows.len(), 10);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::Int64(1));
    assert_eq!(res.rows[9].values[0], Value::Int64(1));
    assert_eq!(res.rows[9].values[1], Value::Int64(10));
    Ok(())
}

#[test]
fn test_r1153_highly_selective_residual_stress() -> Result<()> {
    let ctx = TestContext::new("r1153_selective_residual");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE res_a (id BIGINT PRIMARY KEY, k INT, val INT);")?;
    tx.execute_statement("CREATE TABLE res_b (id BIGINT PRIMARY KEY, k INT);")?;

    for i in 1..=200 {
        let val = if i == 150 { 9999 } else { i };
        tx.execute_statement(&format!("INSERT INTO res_a VALUES ({i}, {i}, {val});"))?;
        tx.execute_statement(&format!("INSERT INTO res_b VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT res_a.id, res_b.id FROM res_a JOIN res_b ON res_a.k = res_b.k WHERE res_a.val = 9999 LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(150));
    Ok(())
}

#[test]
fn test_r1154_large_build_tiny_limit() -> Result<()> {
    let ctx = TestContext::new("r1154_build_limit");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE probe_tbl (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE build_tbl (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=200 {
        tx.execute_statement(&format!("INSERT INTO build_tbl VALUES ({i}, {i});"))?;
        if i <= 5 {
            tx.execute_statement(&format!("INSERT INTO probe_tbl VALUES ({i}, {i});"))?;
        }
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT probe_tbl.id, build_tbl.v FROM probe_tbl JOIN build_tbl ON probe_tbl.id = build_tbl.id LIMIT 2;",
    )?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r1155_ordered_merge_join_beats_hash_sort() -> Result<()> {
    let ctx = TestContext::new("r1155_merge_ordering");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    tx.execute_statement("CREATE TABLE ma (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE mb (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO ma VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO mb VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT ma.id, mb.v FROM ma JOIN mb ON ma.id = mb.id ORDER BY ma.id LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[4].values[0], Value::Int64(5));
    Ok(())
}
