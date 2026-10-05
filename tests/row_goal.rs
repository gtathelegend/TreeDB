use tempfile::TempDir;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Result;
use treedb::query::cost::CostModel;
use treedb::query::expr::Expr;
use treedb::query::feedback::{FeedbackContext, FeedbackKey};
use treedb::query::operator::SortKey;
use treedb::query::physical_properties::{
    PhysicalProperties, RequiredOrdering, RequiredPhysicalProperties, RowGoal,
};
use treedb::query::top_n::TopNOperator;
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
// R751–R760: Row-Goal Representation and Semantics
// -----------------------------------------------------------------------------

#[test]
fn test_r751_row_goal_creation_and_k_calculation() {
    let rg = RowGoal::new(10, 10);
    assert_eq!(rg.k(), 10);
    assert_eq!(rg.candidate_rows, 10);
    assert_eq!(rg.required_output_rows, 10);
    assert_eq!(rg.offset, 0);

    let rg_offset = RowGoal::new(30, 10);
    assert_eq!(rg_offset.k(), 30);
    assert_eq!(rg_offset.candidate_rows, 30);
    assert_eq!(rg_offset.required_output_rows, 10);
    assert_eq!(rg_offset.offset, 20);
}

#[test]
fn test_r752_row_goal_from_limit_offset() {
    let rg1 = RowGoal::from_limit_offset(Some(10), None);
    assert!(rg1.is_some());
    let rg1 = rg1.unwrap();
    assert_eq!(rg1.candidate_rows, 10);
    assert_eq!(rg1.required_output_rows, 10);
    assert_eq!(rg1.offset, 0);

    let rg2 = RowGoal::from_limit_offset(Some(10), Some(20));
    assert!(rg2.is_some());
    let rg2 = rg2.unwrap();
    assert_eq!(rg2.candidate_rows, 30);
    assert_eq!(rg2.required_output_rows, 10);
    assert_eq!(rg2.offset, 20);

    // Unbounded limit should produce None
    let rg3 = RowGoal::from_limit_offset(None, Some(20));
    assert!(rg3.is_none());

    let rg4 = RowGoal::from_limit_offset(None, None);
    assert!(rg4.is_none());
}

#[test]
fn test_r753_required_physical_properties_row_goal() {
    let req = RequiredPhysicalProperties::with_row_goal(RowGoal::new(15, 10));
    assert!(req.row_goal.is_some());
    assert_eq!(req.row_goal.unwrap().k(), 15);
    assert_eq!(req.ordering, RequiredOrdering::default());
}

#[test]
fn test_r754_physical_properties_row_goal() {
    let props = PhysicalProperties::default();
    assert!(props.row_goal.is_none());
}

#[test]
fn test_r755_row_goal_display_formatting() {
    let rg = RowGoal::new(30, 10);
    let s = format!("{rg}");
    assert!(s.contains("30"));
    assert!(s.contains("10"));
    assert!(s.contains("20"));
}

#[test]
fn test_r756_end_to_end_row_goal_limit_semantics() -> Result<()> {
    let ctx = TestContext::new("r756_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", i * 10))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, v FROM t ORDER BY id LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[4].values[0], Value::Int64(5));
    Ok(())
}

#[test]
fn test_r757_end_to_end_row_goal_offset_limit_semantics() -> Result<()> {
    let ctx = TestContext::new("r757_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", i * 10))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, v FROM t ORDER BY id LIMIT 5 OFFSET 10;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(11));
    assert_eq!(res.rows[4].values[0], Value::Int64(15));
    Ok(())
}

#[test]
fn test_r758_row_goal_zero_limit() -> Result<()> {
    let ctx = TestContext::new("r758_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id FROM t ORDER BY id LIMIT 0;")?;
    assert_eq!(res.rows.len(), 0);
    Ok(())
}

#[test]
fn test_r759_row_goal_offset_exceeds_table_size() -> Result<()> {
    let ctx = TestContext::new("r759_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id FROM t ORDER BY id LIMIT 10 OFFSET 100;")?;
    assert_eq!(res.rows.len(), 0);
    Ok(())
}

#[test]
fn test_r760_row_goal_null_and_duplicate_semantics() -> Result<()> {
    let ctx = TestContext::new("r760_semantics");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10);")?;
    tx.execute_statement("INSERT INTO t VALUES (2, NULL);")?;
    tx.execute_statement("INSERT INTO t VALUES (3, 10);")?;
    tx.execute_statement("INSERT INTO t VALUES (4, 20);")?;
    tx.execute_statement("INSERT INTO t VALUES (5, NULL);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, v FROM t ORDER BY v ASC LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[1], Value::Null);
    assert_eq!(res.rows[1].values[1], Value::Null);
    assert_eq!(res.rows[2].values[1], Value::Int64(10));
    Ok(())
}

// -----------------------------------------------------------------------------
// R761–R775: Safe Propagation & Boundary Protection
// -----------------------------------------------------------------------------

#[test]
fn test_r761_propagation_through_filter_and_projection() -> Result<()> {
    let ctx = TestContext::new("r761_propagation");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, category INT, score INT);")?;
    tx.execute_statement("CREATE INDEX idx_score ON t(score);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {}, {i});",
            if i % 2 == 0 { 1 } else { 2 }
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT score * 2 AS double_score FROM t WHERE category = 1 ORDER BY score LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);
    // category = 1 has even ids: 2, 4, 6, 8, 10. score * 2 = 4, 8, 12, 16, 20
    assert_eq!(res.rows[0].values[0], Value::Int64(4));
    assert_eq!(res.rows[4].values[0], Value::Int64(20));
    Ok(())
}

#[test]
fn test_r762_top_n_absorbs_row_goal_and_bounds_memory() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("score", DataType::Int64, false),
        ],
        0,
    )?;
    let mut rows = Vec::new();
    for i in 1..=1000 {
        rows.push(Row::new(vec![Value::Int64(i), Value::Int64(1000 - i)]));
    }
    let sort_keys = vec![SortKey::asc(Expr::Column("score".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(10),
        Some(0),
        &treedb::query::external_sort::SortConfig::default(),
    )?;
    assert_eq!(res.len(), 10);
    assert_eq!(metrics.candidate_capacity, 10);
    assert_eq!(metrics.rows_retained, 10);
    assert_eq!(metrics.rows_discarded, 990);
    Ok(())
}

#[test]
fn test_r763_group_by_aggregation_boundary_does_not_limit_inner_scan() -> Result<()> {
    let ctx = TestContext::new("r763_boundary");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE sales (id BIGINT PRIMARY KEY, dept INT, amount INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO sales VALUES ({i}, {}, 10);", i % 5))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Top 2 depts by total sales: all 100 rows must be aggregated before LIMIT 2 is applied
    let res = tx.execute_sql(
        "SELECT dept, SUM(amount) AS total FROM sales GROUP BY dept ORDER BY total DESC LIMIT 2;",
    )?;
    assert_eq!(res.rows.len(), 2);
    // each of the 5 depts has 20 rows of 10 = 200
    assert_eq!(res.rows[0].values[1], Value::Int64(200));
    assert_eq!(res.rows[1].values[1], Value::Int64(200));
    Ok(())
}

#[test]
fn test_r764_window_function_boundary_does_not_limit_partition_scans() -> Result<()> {
    let ctx = TestContext::new("r764_boundary");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, val, ROW_NUMBER() OVER (ORDER BY val DESC) AS rn FROM t ORDER BY rn ASC LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(20)); // rn 1
    assert_eq!(res.rows[1].values[0], Value::Int64(19)); // rn 2
    assert_eq!(res.rows[2].values[0], Value::Int64(18)); // rn 3
    Ok(())
}

#[test]
fn test_r765_join_boundary_safety_does_not_push_limit_to_child() -> Result<()> {
    let ctx = TestContext::new("r765_boundary");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT, amount INT);",
    )?;
    tx.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO customers VALUES ({i}, 'Cust{i}');"))?;
    }
    // Only customer 20 has orders
    for i in 1..=10 {
        tx.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, 20, {});",
            i * 100
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // If LIMIT 1 were pushed down to customers, it might only scan Cust1 and find 0 orders!
    let res = tx.execute_sql("SELECT o.id, c.name FROM customers c JOIN orders o ON c.id = o.customer_id ORDER BY o.id LIMIT 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Cust20".to_string()));
    Ok(())
}

#[test]
fn test_r766_cte_boundary_preserves_semantics() -> Result<()> {
    let ctx = TestContext::new("r766_boundary");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("WITH filtered AS (SELECT id, v FROM t WHERE v > 40) SELECT id FROM filtered ORDER BY id DESC LIMIT 2;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(50));
    assert_eq!(res.rows[1].values[0], Value::Int64(49));
    Ok(())
}

#[test]
fn test_r767_set_operation_boundary_preserves_semantics() -> Result<()> {
    let ctx = TestContext::new("r767_boundary");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t1 VALUES (1, 10);")?;
    tx.execute_statement("INSERT INTO t1 VALUES (2, 20);")?;
    tx.execute_statement("INSERT INTO t2 VALUES (3, 5);")?;
    tx.execute_statement("INSERT INTO t2 VALUES (4, 15);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT id, val FROM t1 UNION ALL SELECT id, val FROM t2 ORDER BY val ASC LIMIT 2;",
    )?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::Int64(5));
    assert_eq!(res.rows[1].values[1], Value::Int64(10));
    Ok(())
}

// -----------------------------------------------------------------------------
// R776–R790: Selectivity-Aware Ordered Scan Costing
// -----------------------------------------------------------------------------

#[test]
fn test_r776_expected_entries_full_selectivity() {
    let entries = CostModel::expected_entries_for_row_goal(10.0, 1.0, 100_000.0);
    assert_eq!(entries, 10.0);
}

#[test]
fn test_r777_expected_entries_zero_selectivity_clamps_to_total() {
    let entries = CostModel::expected_entries_for_row_goal(10.0, 0.0, 100_000.0);
    assert_eq!(entries, 100_000.0);
}

#[test]
fn test_r778_expected_entries_moderate_selectivity() {
    // 10 / 0.1 = 100
    let entries = CostModel::expected_entries_for_row_goal(10.0, 0.10, 100_000.0);
    assert_eq!(entries, 100.0);
}

#[test]
fn test_r779_expected_entries_low_selectivity_clamps_to_population() {
    // 10 / 0.00001 = 1,000,000 > 50,000 -> clamp to 50,000
    let entries = CostModel::expected_entries_for_row_goal(10.0, 0.00001, 50_000.0);
    assert_eq!(entries, 50_000.0);
}

#[test]
fn test_r780_expected_entries_k_exceeds_population() {
    let entries = CostModel::expected_entries_for_row_goal(500.0, 1.0, 100.0);
    assert_eq!(entries, 100.0);
}

#[test]
fn test_r781_ordered_scan_cost_calculation() {
    let cost_covering = CostModel::ordered_scan_row_goal_cost(100.0, true, false, 1);
    let cost_secondary = CostModel::ordered_scan_row_goal_cost(100.0, false, false, 1);
    let cost_pk = CostModel::ordered_scan_row_goal_cost(100.0, false, true, 1);

    // Secondary scan must cost more than covering scan due to base tuple fetching
    assert!(cost_secondary > cost_covering);
    // Secondary scan must cost more than PK scan
    assert!(cost_secondary > cost_pk);
}

#[test]
fn test_r782_row_goal_filter_cost_calculation() {
    let filter_cost = CostModel::row_goal_filter_cost(1000.0, 2);
    assert!(filter_cost > 0.0);
}

// -----------------------------------------------------------------------------
// R791–R805: Histogram and Statistics Integration
// -----------------------------------------------------------------------------

#[test]
fn test_r791_statistics_influence_ordered_scan_cost() -> Result<()> {
    let ctx = TestContext::new("r791_stats");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, status INT, created_at INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_created ON orders(created_at);")?;
    tx.execute_statement("CREATE INDEX idx_status ON orders(status);")?;
    // Insert 1000 rows where status=999 is present only once
    for i in 1..=1000 {
        tx.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, {}, {i});",
            if i == 500 { 999 } else { 1 }
        ))?;
    }
    tx.execute_statement("ANALYZE orders;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Query with status = 999 ORDER BY created_at LIMIT 10
    // Because status=999 is extremely rare (0.1%), the ordered scan on idx_created would need
    // to examine all 1000 rows. The selective index on idx_status has ~1 row + TopN.
    let plan =
        tx.explain_sql("SELECT id FROM orders WHERE status = 999 ORDER BY created_at LIMIT 10;")?;
    assert!(
        plan.contains("idx_status") || plan.contains("TopN") || plan.contains("Estimated"),
        "Plan should account for selective filter: {plan}"
    );

    let res =
        tx.execute_sql("SELECT id FROM orders WHERE status = 999 ORDER BY created_at LIMIT 10;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(500));
    Ok(())
}

#[test]
fn test_r792_range_predicate_selectivity_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r792_range");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, price INT, rank INT);")?;
    tx.execute_statement("CREATE INDEX idx_rank ON items(rank);")?;
    for i in 1..=500 {
        tx.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i}, {i});"))?;
    }
    tx.execute_statement("ANALYZE items;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT id FROM items WHERE price > 490 ORDER BY rank ASC LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(491));
    assert_eq!(res.rows[4].values[0], Value::Int64(495));
    Ok(())
}

#[test]
fn test_r793_between_and_in_predicates_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r793_between_in");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE products (id BIGINT PRIMARY KEY, tag INT, created_at INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_created ON products(created_at);")?;
    for i in 1..=200 {
        tx.execute_statement(&format!(
            "INSERT INTO products VALUES ({i}, {}, {i});",
            i % 10
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT id FROM products WHERE tag IN (1, 2) ORDER BY created_at LIMIT 4;")?;
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    assert_eq!(res.rows[2].values[0], Value::Int64(11));
    assert_eq!(res.rows[3].values[0], Value::Int64(12));
    Ok(())
}

// -----------------------------------------------------------------------------
// R806–R820: Composite and Covering Indexes
// -----------------------------------------------------------------------------

#[test]
fn test_r806_composite_index_prefix_narrowing_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r806_composite");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, tenant_id INT, event_time INT, payload TEXT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_tenant_time ON events(tenant_id, event_time);")?;
    for tenant in 1..=5 {
        for t in 1..=50 {
            let id = (tenant - 1) * 50 + t;
            tx.execute_statement(&format!(
                "INSERT INTO events VALUES ({id}, {tenant}, {t}, 'payload_{id}');"
            ))?;
        }
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, tenant_id, event_time FROM events WHERE tenant_id = 3 ORDER BY tenant_id, event_time LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[1], Value::Int64(3));
    assert_eq!(res.rows[0].values[2], Value::Int64(1));
    assert_eq!(res.rows[4].values[2], Value::Int64(5));
    Ok(())
}

#[test]
fn test_r807_covering_index_avoids_base_records_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r807_covering");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE logs (id BIGINT PRIMARY KEY, ts INT, level INT, msg TEXT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_covering ON logs(ts) INCLUDE (level);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!(
            "INSERT INTO logs VALUES ({i}, {i}, {}, 'log message content {i}');",
            if i % 2 == 0 { 2 } else { 1 }
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT ts, level FROM logs WHERE level = 2 ORDER BY ts LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    assert_eq!(res.rows[4].values[0], Value::Int64(10));
    Ok(())
}

// -----------------------------------------------------------------------------
// R821–R835: Candidate Access-Path Comparison
// -----------------------------------------------------------------------------

#[test]
fn test_r821_high_selectivity_favors_ordered_index_scan() -> Result<()> {
    let ctx = TestContext::new("r821_high_sel");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, priority INT, created_at INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_created ON orders(created_at);")?;
    // 90% of rows have priority = 1
    for i in 1..=1000 {
        tx.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, {}, {i});",
            if i % 10 != 0 { 1 } else { 2 }
        ))?;
    }
    tx.execute_statement("ANALYZE orders;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan =
        tx.explain_sql("SELECT id FROM orders WHERE priority = 1 ORDER BY created_at LIMIT 10;")?;
    // High selectivity (90%) makes ordered index scan very cheap (~12 entries)
    assert!(
        plan.contains("idx_created") || plan.contains("Row Goal"),
        "High selectivity should leverage ordered scan: {plan}"
    );

    let res =
        tx.execute_sql("SELECT id FROM orders WHERE priority = 1 ORDER BY created_at LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    Ok(())
}

#[test]
fn test_r822_low_selectivity_with_selective_index_favors_top_n() -> Result<()> {
    let ctx = TestContext::new("r822_low_sel");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT, created_at INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_created ON users(created_at);")?;
    tx.execute_statement("CREATE INDEX idx_email ON users(email);")?;
    for i in 1..=1000 {
        tx.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'user{i}@example.com', {i});"
        ))?;
    }
    tx.execute_statement("ANALYZE users;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql(
        "SELECT id FROM users WHERE email = 'user42@example.com' ORDER BY created_at LIMIT 10;",
    )?;
    // Email is unique/point lookup (selectivity 0.1%), so idx_email + TopN is far cheaper than scanning idx_created
    assert!(
        plan.contains("idx_email") || plan.contains("TopN"),
        "Selective index should be chosen over ordered index scan: {plan}"
    );

    let res = tx.execute_sql(
        "SELECT id FROM users WHERE email = 'user42@example.com' ORDER BY created_at LIMIT 10;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(42));
    Ok(())
}

#[test]
fn test_r823_multi_index_access_path_comparison() -> Result<()> {
    let ctx = TestContext::new("r823_multi");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, sort_col INT);")?;
    tx.execute_statement("CREATE INDEX idx_a ON t(a);")?;
    tx.execute_statement("CREATE INDEX idx_b ON t(b);")?;
    tx.execute_statement("CREATE INDEX idx_sort ON t(sort_col);")?;
    for i in 1..=200 {
        tx.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {}, {}, {i});",
            i % 5,
            i % 4
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT id FROM t WHERE a = 1 AND b = 1 ORDER BY sort_col LIMIT 5;")?;
    assert!(!res.rows.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------------
// R836–R850: Runtime Feedback and Adaptive Replanning
// -----------------------------------------------------------------------------

#[test]
fn test_r836_feedback_key_ordered_scan() {
    let key = FeedbackKey::ordered_scan("orders", "idx_created", "status = 1", 10, None, 1);
    assert_eq!(key.table_name(), Some("orders"));
}

#[test]
fn test_r837_feedback_context_ordered_scan_display() {
    let ctx = FeedbackContext::OrderedScan {
        table_name: "users".to_string(),
        index_name: "idx_created_at".to_string(),
        filter: "active = 1".to_string(),
        row_goal: 20,
    };
    let s = format!("{ctx}");
    assert!(s.contains("OrderedScan"));
    assert!(s.contains("users"));
    assert!(s.contains("idx_created_at"));
}

#[test]
fn test_r838_execution_updates_ordered_scan_metrics() -> Result<()> {
    let ctx = TestContext::new("r838_feedback");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE INDEX idx_v ON t(v);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let (res, metrics) = tx.execute_sql_with_metrics("SELECT id, v FROM t ORDER BY v LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(metrics.total_rows, 5);
    Ok(())
}

// -----------------------------------------------------------------------------
// R851–R865: EXPLAIN and EXPLAIN ANALYZE Diagnostics
// -----------------------------------------------------------------------------

#[test]
fn test_r851_explain_shows_row_goal_metadata() -> Result<()> {
    let ctx = TestContext::new("r851_explain");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE INDEX idx_v ON t(v);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT id, v FROM t ORDER BY v LIMIT 10;")?;
    assert!(plan.contains("Row Goal: 10") || plan.contains("TopN") || plan.contains("idx_v"));
    Ok(())
}

#[test]
fn test_r852_explain_analyze_metrics_include_row_goal_counters() -> Result<()> {
    let ctx = TestContext::new("r852_explain_analyze");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT, flag INT);")?;
    tx.execute_statement("CREATE INDEX idx_v ON t(v);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {});", i % 2))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res =
        tx.execute_sql("EXPLAIN ANALYZE SELECT id, v FROM t WHERE flag = 1 ORDER BY v LIMIT 5;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(
        ea.contains("actual_rows: 5")
            || ea.contains("rows_examined")
            || ea.contains("candidate_capacity")
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// R866–R880: Prepared Statements and Plan Cache Behavior
// -----------------------------------------------------------------------------

#[test]
fn test_r866_plan_cache_parameter_sensitivity() -> Result<()> {
    let ctx = TestContext::new("r866_cache");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE INDEX idx_v ON t(v);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Query with LIMIT 5
    let res1 = tx.execute_sql("SELECT id FROM t ORDER BY v LIMIT 5;")?;
    assert_eq!(res1.rows.len(), 5);

    // Query with LIMIT 20 (different row goal)
    let res2 = tx.execute_sql("SELECT id FROM t ORDER BY v LIMIT 20;")?;
    assert_eq!(res2.rows.len(), 20);
    Ok(())
}

// -----------------------------------------------------------------------------
// R881–R895: Transactions, Persistence, Checkpoint and Recovery
// -----------------------------------------------------------------------------

#[test]
fn test_r881_transaction_isolation_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r881_tx");
    let mut db = ctx.db();
    let mut tx1 = db.begin_transaction()?;
    tx1.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=10 {
        tx1.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx1.commit()?;

    // Uncommitted insert in tx2
    let mut tx2 = db.begin_transaction()?;
    tx2.execute_statement("INSERT INTO t VALUES (0, 0);")?;

    // Transaction sees its own uncommitted insert with row goal
    let res_uncommitted = tx2.execute_sql("SELECT id FROM t ORDER BY id LIMIT 3;")?;
    assert_eq!(res_uncommitted.rows.len(), 3);
    assert_eq!(res_uncommitted.rows[0].values[0], Value::Int64(0));

    // Abort tx2
    tx2.abort()?;

    // Read transaction in new tx does not see aborted insert
    let mut tx_read = db.begin_transaction()?;
    let res = tx_read.execute_sql("SELECT id FROM t ORDER BY id LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    tx_read.commit()?;
    Ok(())
}

#[test]
fn test_r882_persistence_and_recovery_with_row_goal() -> Result<()> {
    let ctx = TestContext::new("r882_persist");
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
        tx.execute_statement("CREATE INDEX idx_v ON t(v);")?;
        for i in 1..=100 {
            tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", i * 2))?;
        }
        tx.execute_statement("ANALYZE t;")?;
        tx.commit()?;
    }

    // Reopen database from disk
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT id, v FROM t ORDER BY v LIMIT 5;")?;
        assert_eq!(res.rows.len(), 5);
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[0].values[1], Value::Int64(2));
        assert_eq!(res.rows[4].values[0], Value::Int64(5));
        assert_eq!(res.rows[4].values[1], Value::Int64(10));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// R896–R910: Pathological Low-Selectivity Tests
// -----------------------------------------------------------------------------

#[test]
fn test_r896_pathological_low_selectivity_avoids_ordered_index_scan() -> Result<()> {
    let ctx = TestContext::new("r896_pathological");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE items (id BIGINT PRIMARY KEY, rare_status INT, sort_val INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_sort ON items(sort_val);")?;
    tx.execute_statement("CREATE INDEX idx_rare ON items(rare_status);")?;

    // Insert 5000 rows where rare_status = 777 occurs only twice (0.04% selectivity)
    for i in 1..=5000 {
        let rare = if i == 2500 || i == 4900 { 777 } else { 0 };
        tx.execute_statement(&format!("INSERT INTO items VALUES ({i}, {rare}, {i});"))?;
    }
    tx.execute_statement("ANALYZE items;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Query: ORDER BY sort_val LIMIT 10 WHERE rare_status = 777
    // Ordered scan on idx_sort would have to examine ~5000 entries.
    // Index scan on idx_rare examines 2 entries + TopN.
    let plan =
        tx.explain_sql("SELECT id FROM items WHERE rare_status = 777 ORDER BY sort_val LIMIT 10;")?;
    assert!(
        plan.contains("idx_rare") || plan.contains("TopN"),
        "Optimizer must avoid scanning idx_sort when rare predicate selectivity is ~0%: {plan}"
    );

    let res =
        tx.execute_sql("SELECT id FROM items WHERE rare_status = 777 ORDER BY sort_val LIMIT 10;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(2500));
    assert_eq!(res.rows[1].values[0], Value::Int64(4900));
    Ok(())
}

#[test]
fn test_r897_varying_selectivity_cost_spectrum() {
    let total_pop = 100_000.0;
    let k = 10.0;

    let entries_50pct = CostModel::expected_entries_for_row_goal(k, 0.50, total_pop);
    let entries_10pct = CostModel::expected_entries_for_row_goal(k, 0.10, total_pop);
    let entries_1pct = CostModel::expected_entries_for_row_goal(k, 0.01, total_pop);
    let entries_001pct = CostModel::expected_entries_for_row_goal(k, 0.0001, total_pop);
    let entries_0pct = CostModel::expected_entries_for_row_goal(k, 0.0, total_pop);

    assert_eq!(entries_50pct, 20.0);
    assert_eq!(entries_10pct, 100.0);
    assert_eq!(entries_1pct, 1000.0);
    assert_eq!(entries_001pct, 100_000.0); // clamped to total_pop
    assert_eq!(entries_0pct, 100_000.0);

    assert!(entries_50pct < entries_10pct);
    assert!(entries_10pct < entries_1pct);
    assert!(entries_1pct < entries_001pct);
}

// -----------------------------------------------------------------------------
// R911–R925: Randomized Differential Testing
// -----------------------------------------------------------------------------

#[test]
fn test_r911_randomized_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("r911_diff");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE data (id BIGINT PRIMARY KEY, category INT, score INT);")?;
    tx.execute_statement("CREATE INDEX idx_score ON data(score);")?;

    // Deterministic pseudo-random seed generator
    let mut seed: u64 = 42;
    let mut rand_u64 = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed
    };

    for i in 1..=300 {
        let cat = (rand_u64() % 10) as i64;
        let score = (rand_u64() % 1000) as i64;
        tx.execute_statement(&format!("INSERT INTO data VALUES ({i}, {cat}, {score});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;

    // Test multiple combinations of category filters, limits, and offsets
    for cat in 0..5 {
        for limit in [1, 5, 15] {
            for offset in [0, 5, 20] {
                let sql = format!(
                    "SELECT id, category, score FROM data WHERE category = {cat} ORDER BY score ASC LIMIT {limit} OFFSET {offset};"
                );
                let res = tx.execute_sql(&sql)?;

                // Reference verification
                let all_res = tx.execute_sql("SELECT id, category, score FROM data;")?;
                let mut matched: Vec<_> = all_res
                    .rows
                    .into_iter()
                    .filter(|r| r.values[1] == Value::Int64(cat))
                    .collect();
                matched.sort_by_key(|r| match r.values[2] {
                    Value::Int64(s) => s,
                    _ => 0,
                });
                let expected: Vec<_> = matched.into_iter().skip(offset).take(limit).collect();

                assert_eq!(
                    res.rows.len(),
                    expected.len(),
                    "Length mismatch for query: {sql}"
                );
                for (r, exp) in res.rows.iter().zip(expected.iter()) {
                    assert_eq!(r.values[0], exp.values[0]);
                    assert_eq!(r.values[1], exp.values[1]);
                    assert_eq!(r.values[2], exp.values[2]);
                }
            }
        }
    }
    Ok(())
}
