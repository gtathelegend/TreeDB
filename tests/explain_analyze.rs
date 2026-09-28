use std::path::PathBuf;
use treedb::engine::types::Value;
use treedb::error::{Error, Result};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "test_ea_{}_{}_{}.treedb",
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

fn create_standard_test_tables(db: &mut TreeDb) -> Result<()> {
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE departments (dept_id BIGINT PRIMARY KEY, dept_name TEXT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT, dept_id BIGINT);",
    )?;
    txn.execute_statement("CREATE UNIQUE INDEX idx_users_name ON users (name);")?;
    txn.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    txn.execute_statement("CREATE INDEX idx_users_dept ON users (dept_id);")?;

    // Populate departments
    txn.execute_statement(
        "INSERT INTO departments (dept_id, dept_name) VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'HR');",
    )?;

    // Populate 300 users
    for i in 1..=300 {
        let age = 20 + (i % 50);
        let dept = (i % 3) + 1;
        txn.execute_statement(&format!(
            "INSERT INTO users (id, name, age, dept_id) VALUES ({i}, 'user_{i}@corp.com', {age}, {dept});"
        ))?;
    }
    txn.commit()?;

    // Collect statistics
    let mut txn = db.begin_transaction()?;
    txn.collect_statistics("users")?;
    txn.collect_statistics("departments")?;
    txn.commit()?;

    Ok(())
}

// ============================================================================
// 1. Basic Operator Instrumentation Tests
// ============================================================================

#[test]
fn test_explain_analyze_table_scan_and_filter() -> Result<()> {
    let ctx = TestContext::new("scan_filter");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics("SELECT * FROM users WHERE age = 68;")?;
    assert_eq!(res.rows.len(), 6);
    assert_eq!(metrics.total_rows, 6);

    let root = metrics.root_operator.expect("root operator exists");
    assert_eq!(root.actual_rows, 6);

    // Verify EXPLAIN ANALYZE SQL output
    let explain_res = txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM users WHERE age = 68;")?;
    let plan_text = match &explain_res.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("TableScan"));
    assert!(plan_text.contains("actual_rows: 6"));
    assert!(plan_text.contains("cost:"));
    assert!(plan_text.contains("estimated_rows:"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_primary_key_point_lookup() -> Result<()> {
    let ctx = TestContext::new("pk_lookup");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics("SELECT * FROM users WHERE id = 42;")?;
    assert_eq!(res.rows.len(), 1);

    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 1);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM users WHERE id = 42;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("PrimaryPointLookup(key=42)"));
    assert!(plan_text.contains("actual_rows: 1"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_primary_key_range_scan() -> Result<()> {
    let ctx = TestContext::new("pk_range");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_range (id BIGINT PRIMARY KEY, name TEXT);")?;
    for i in 1..=50 {
        txn.execute_statement(&format!(
            "INSERT INTO t_range (id, name) VALUES ({i}, 'item_{i}');"
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT * FROM t_range WHERE id >= 10 AND id < 20;")?;
    assert_eq!(res.rows.len(), 10);

    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 10);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM t_range WHERE id >= 10 AND id < 20;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("PrimaryRangeScan"));
    assert!(plan_text.contains("actual_rows: 10"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_secondary_index_scan() -> Result<()> {
    let ctx = TestContext::new("sec_scan");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT * FROM users WHERE name = 'user_42@corp.com';")?;
    assert_eq!(res.rows.len(), 1);

    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 1);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM users WHERE name = 'user_42@corp.com';")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("SecondaryIndexScan"));
    assert!(plan_text.contains("actual_rows: 1"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_sort_limit_offset() -> Result<()> {
    let ctx = TestContext::new("sort_limit");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT id, name, age FROM users ORDER BY age DESC, id ASC LIMIT 2 OFFSET 1;",
    )?;
    assert_eq!(res.rows.len(), 2);

    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 2);
    assert_eq!(root.kind, treedb::query::OperatorKind::LimitOffset);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT id, name, age FROM users ORDER BY age DESC, id ASC LIMIT 2 OFFSET 1;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("LimitOffset (limit: Some(2), offset: Some(1))"));
    assert!(plan_text.contains("Sort [age Descending, id Ascending]"));

    txn.commit()?;
    Ok(())
}

// ============================================================================
// 2. Relational Operations: Joins, Aggregates, Windows, CTEs, Set Operations
// ============================================================================

#[test]
fn test_explain_analyze_inner_and_left_joins() -> Result<()> {
    let ctx = TestContext::new("joins");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    // Inner join
    let (res_inner, metrics_inner) = txn.execute_sql_with_metrics(
        "SELECT u.name, d.dept_name FROM users u JOIN departments d ON u.dept_id = d.dept_id WHERE u.id <= 5;",
    )?;
    assert_eq!(res_inner.rows.len(), 5);
    assert_eq!(metrics_inner.total_rows, 5);

    let plan_inner = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT u.name, d.dept_name FROM users u JOIN departments d ON u.dept_id = d.dept_id WHERE u.id <= 5;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_inner.contains("NestedLoopJoin (Inner"));
    assert!(plan_inner.contains("actual_rows: 5"));

    // Left outer join
    let (res_left, metrics_left) = txn.execute_sql_with_metrics(
        "SELECT u.name, d.dept_name FROM users u LEFT JOIN departments d ON u.dept_id = d.dept_id WHERE u.id <= 5;",
    )?;
    assert_eq!(res_left.rows.len(), 5);
    assert_eq!(metrics_left.total_rows, 5);

    let plan_left = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT u.name, d.dept_name FROM users u LEFT JOIN departments d ON u.dept_id = d.dept_id WHERE u.id <= 5;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_left.contains("NestedLoopJoin (Left"));
    assert!(plan_left.contains("actual_rows: 5"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_aggregation_and_having() -> Result<()> {
    let ctx = TestContext::new("agg_having");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT dept_id, COUNT(id) AS cnt, AVG(age) AS avg_age FROM users GROUP BY dept_id HAVING COUNT(id) >= 2;",
    )?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(metrics.total_rows, 3);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT dept_id, COUNT(id) AS cnt, AVG(age) AS avg_age FROM users GROUP BY dept_id HAVING COUNT(id) >= 2;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("Aggregate (group_by: [dept_id]"));
    assert!(plan_text.contains("Having [(COUNT(id) >= 2)]"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_window_functions() -> Result<()> {
    let ctx = TestContext::new("window");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT id, name, dept_id, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY id ASC) as rn FROM users WHERE id <= 10;",
    )?;
    assert_eq!(res.rows.len(), 10);
    assert_eq!(metrics.total_rows, 10);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT id, name, dept_id, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY id ASC) as rn FROM users WHERE id <= 10;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("Window"));
    assert!(plan_text.contains("ROW_NUMBER()"));
    assert!(plan_text.contains("Partition By:"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_subquery_and_ctes() -> Result<()> {
    let ctx = TestContext::new("subquery_cte");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    // Subquery
    let (res_sub, metrics_sub) = txn.execute_sql_with_metrics(
        "SELECT id, name FROM users WHERE dept_id IN (SELECT dept_id FROM departments WHERE dept_name = 'Engineering') AND id <= 10;",
    )?;
    assert_eq!(res_sub.rows.len(), 3);
    assert_eq!(metrics_sub.total_rows, 3);

    // Non-recursive CTE
    let (res_cte, metrics_cte) = txn.execute_sql_with_metrics(
        "WITH eng AS (SELECT id, name FROM users WHERE dept_id = 1) SELECT * FROM eng WHERE id = 3;",
    )?;
    assert_eq!(res_cte.rows.len(), 1);
    assert_eq!(metrics_cte.total_rows, 1);

    let plan_cte = match &txn
        .execute_sql("EXPLAIN ANALYZE WITH eng AS (SELECT id, name FROM users WHERE dept_id = 1) SELECT * FROM eng WHERE id = 3;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_cte.contains("CTE eng"));
    assert!(plan_cte.contains("Main Query"));

    // Recursive CTE
    let (res_rec, metrics_rec) = txn.execute_sql_with_metrics(
        "WITH RECURSIVE cnt(x) AS (SELECT dept_id FROM departments WHERE dept_id = 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 3) SELECT * FROM cnt;",
    )?;
    assert_eq!(res_rec.rows.len(), 3);
    assert_eq!(metrics_rec.total_rows, 3);

    let plan_rec = match &txn
        .execute_sql("EXPLAIN ANALYZE WITH RECURSIVE cnt(x) AS (SELECT dept_id FROM departments WHERE dept_id = 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 3) SELECT * FROM cnt;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_rec.contains("Recursive CTE cnt"));
    assert!(plan_rec.contains("Anchor"));
    assert!(plan_rec.contains("Recursive Member"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_set_operations() -> Result<()> {
    let ctx = TestContext::new("set_ops");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (res_union, metrics_union) = txn.execute_sql_with_metrics(
        "SELECT id FROM users WHERE id <= 2 UNION SELECT id FROM users WHERE id >= 299;",
    )?;
    assert_eq!(res_union.rows.len(), 4);
    assert_eq!(metrics_union.total_rows, 4);

    let plan_union = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT id FROM users WHERE id <= 2 UNION SELECT id FROM users WHERE id >= 299;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_union.contains("Set Operation: UNION"));
    assert!(plan_union.contains("Left"));
    assert!(plan_union.contains("Right"));

    txn.commit()?;
    Ok(())
}

// ============================================================================
// 3. Cardinality Diagnostics & Statistics Validation
// ============================================================================

#[test]
fn test_cardinality_diagnostics_and_error_ratio() -> Result<()> {
    let ctx = TestContext::new("cardinality_diag");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let (_res, metrics) = txn.execute_sql_with_metrics("SELECT * FROM users;")?;
    assert_eq!(metrics.total_rows, 300);

    let ratio = metrics.root_estimate_error_ratio();
    assert!(ratio.is_some());
    let r = ratio.unwrap();
    // Estimate is 300.0 and actual is 300, ratio should be 1.0
    assert!((r - 1.0).abs() < 1e-4);

    // Estimate ratio helper
    assert_eq!(treedb::query::estimate_error_ratio(5, Some(5.0)), Some(1.0));
    assert_eq!(treedb::query::estimate_error_ratio(5, Some(0.0)), None);
    assert_eq!(treedb::query::estimate_error_ratio(5, None), None);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_missing_and_stale_statistics_diagnostics() -> Result<()> {
    let ctx = TestContext::new("missing_stats");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_unanal (id BIGINT PRIMARY KEY, val BIGINT);")?;
    txn.execute_statement("INSERT INTO t_unanal (id, val) VALUES (1, 10), (2, 20), (3, 30);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // Query without ANALYZE (uses fallback statistics)
    let (res, metrics) = txn.execute_sql_with_metrics("SELECT * FROM t_unanal WHERE val > 15;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(metrics.total_rows, 2);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM t_unanal WHERE val > 15;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("actual_rows: 2"));

    txn.commit()?;
    Ok(())
}

// ============================================================================
// 4. Transaction Semantics & Visibility
// ============================================================================

#[test]
fn test_explain_analyze_transaction_visibility_and_rollback() -> Result<()> {
    let ctx = TestContext::new("txn_vis");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;
    // 1. Initial count in EXPLAIN ANALYZE is 300
    let (res1, metrics1) = txn.execute_sql_with_metrics("SELECT * FROM users;")?;
    assert_eq!(res1.rows.len(), 300);
    assert_eq!(metrics1.total_rows, 300);

    // 2. Uncommitted INSERT inside this transaction
    txn.execute_statement(
        "INSERT INTO users (id, name, age, dept_id) VALUES (301, 'user_301@corp.com', 28, 1);",
    )?;

    // 3. EXPLAIN ANALYZE sees the uncommitted row!
    let (res2, metrics2) = txn.execute_sql_with_metrics("SELECT * FROM users;")?;
    assert_eq!(res2.rows.len(), 301);
    assert_eq!(metrics2.total_rows, 301);

    let plan_text = match &txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM users;")?
        .rows[0]
        .values[0]
    {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(plan_text.contains("actual_rows: 301"));

    // 4. Rollback transaction
    txn.abort()?;

    // 5. In a new transaction, row 301 is gone
    let mut txn2 = db.begin_transaction()?;
    let (res3, metrics3) = txn2.execute_sql_with_metrics("SELECT * FROM users;")?;
    assert_eq!(res3.rows.len(), 300);
    assert_eq!(metrics3.total_rows, 300);
    txn2.commit()?;

    Ok(())
}

// ============================================================================
// 5. Safety, Distinction & DML Policy
// ============================================================================

#[test]
fn test_explain_vs_explain_analyze_execution_distinction() -> Result<()> {
    let ctx = TestContext::new("explain_distinction");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;

    // EXPLAIN does NOT execute the query
    let plain_explain = txn.execute_sql("EXPLAIN SELECT * FROM users WHERE age > 20;")?;
    let plain_text = match &plain_explain.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(!plain_text.contains("actual_rows:"));

    // EXPLAIN ANALYZE DOES execute and formats actual metrics
    let analyze_explain = txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM users WHERE age > 20;")?;
    let analyze_text = match &analyze_explain.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };
    assert!(analyze_text.contains("actual_rows:"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_explain_analyze_dml_rejection() -> Result<()> {
    let ctx = TestContext::new("dml_rejection");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let mut txn = db.begin_transaction()?;

    // EXPLAIN ANALYZE on DML must return structured Error::UnsupportedExplainTarget
    let err_insert =
        txn.execute_statement("EXPLAIN ANALYZE INSERT INTO users (id, name) VALUES (1000, 'Z');");
    assert!(matches!(
        err_insert,
        Err(Error::UnsupportedExplainTarget(_))
    ));

    let err_update =
        txn.execute_statement("EXPLAIN ANALYZE UPDATE users SET age = 31 WHERE id = 1;");
    assert!(matches!(
        err_update,
        Err(Error::UnsupportedExplainTarget(_))
    ));

    let err_delete = txn.execute_statement("EXPLAIN ANALYZE DELETE FROM users WHERE id = 1;");
    assert!(matches!(
        err_delete,
        Err(Error::UnsupportedExplainTarget(_))
    ));

    let err_create = txn.execute_statement("EXPLAIN ANALYZE CREATE TABLE test (id INT);");
    assert!(matches!(
        err_create,
        Err(Error::UnsupportedExplainTarget(_))
    ));

    let err_drop = txn.execute_statement("EXPLAIN ANALYZE DROP TABLE users;");
    assert!(matches!(err_drop, Err(Error::UnsupportedExplainTarget(_))));

    txn.commit()?;
    Ok(())
}

// ============================================================================
// 6. Direct TreeDb API Tests
// ============================================================================

#[test]
fn test_treedb_execute_sql_with_metrics_api() -> Result<()> {
    let ctx = TestContext::new("direct_api");
    let mut db = ctx.db();
    create_standard_test_tables(&mut db)?;

    let (res, metrics) =
        db.execute_sql_with_metrics("SELECT id, name FROM users WHERE id <= 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(metrics.total_rows, 3);
    assert!(metrics.root_operator.is_some());
    Ok(())
}

// ============================================================================
// 7. Randomized Differential Testing
// ============================================================================

#[test]
fn test_randomized_explain_analyze_oracle() -> Result<()> {
    let ctx = TestContext::new("oracle");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE oracle_t (id BIGINT PRIMARY KEY, a BIGINT, b TEXT, c BIGINT);",
    )?;
    txn.execute_statement("CREATE INDEX idx_oracle_a ON oracle_t (a);")?;

    // Insert 50 rows with varied, null, and duplicate values
    for i in 1..=50 {
        let a_val = if i % 7 == 0 {
            "NULL".to_string()
        } else {
            (i % 10).to_string()
        };
        let b_val = if i % 5 == 0 {
            "NULL".to_string()
        } else {
            format!("'val_{}'", i % 8)
        };
        let c_val = if i % 3 == 0 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        txn.execute_statement(&format!(
            "INSERT INTO oracle_t (id, a, b, c) VALUES ({i}, {a_val}, {b_val}, {c_val});"
        ))?;
    }
    txn.commit()?;

    let queries = vec![
        "SELECT * FROM oracle_t WHERE id = 25;",
        "SELECT * FROM oracle_t WHERE id >= 10 AND id < 20;",
        "SELECT * FROM oracle_t WHERE a = 3;",
        "SELECT * FROM oracle_t WHERE a IS NULL;",
        "SELECT * FROM oracle_t WHERE a IS NOT NULL AND c > 100;",
        "SELECT a, COUNT(*), AVG(c) FROM oracle_t WHERE a IS NOT NULL GROUP BY a HAVING COUNT(*) >= 2;",
        "SELECT id, a, b FROM oracle_t ORDER BY a DESC, id ASC LIMIT 10 OFFSET 5;",
        "SELECT id, a, ROW_NUMBER() OVER (PARTITION BY a ORDER BY id ASC) FROM oracle_t WHERE a IS NOT NULL;",
        "SELECT * FROM oracle_t WHERE id IN (SELECT id FROM oracle_t WHERE a = 2);",
    ];

    let mut txn = db.begin_transaction()?;
    for sql in queries {
        let reference_result = txn.execute_sql(sql)?;
        let (metrics_result, metrics) = txn.execute_sql_with_metrics(sql)?;

        // Verify result row count and values match perfectly
        assert_eq!(
            reference_result.rows.len(),
            metrics_result.rows.len(),
            "Row count mismatch for SQL: {sql}"
        );
        assert_eq!(
            metrics.total_rows,
            reference_result.rows.len(),
            "Total rows metric mismatch for SQL: {sql}"
        );

        for (r_ref, r_met) in reference_result.rows.iter().zip(metrics_result.rows.iter()) {
            assert_eq!(
                r_ref.values, r_met.values,
                "Row value mismatch for SQL: {sql}"
            );
        }

        // Verify EXPLAIN ANALYZE produces valid output containing actual_rows
        let explain_analyze_res = txn.execute_sql(&format!("EXPLAIN ANALYZE {sql}"))?;
        let plan_str = match &explain_analyze_res.rows[0].values[0] {
            Value::String(s) => s,
            _ => panic!("expected string output"),
        };
        assert!(
            plan_str.contains("actual_rows:"),
            "EXPLAIN ANALYZE missing actual_rows for SQL: {sql}"
        );
    }
    txn.commit()?;

    Ok(())
}

#[test]
fn test_phase38_semantic_smoke_tests() -> Result<()> {
    let ctx = TestContext::new("phase38_smoke");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT, amount INT);",
    )?;

    txn.execute_statement(
        "INSERT INTO customers (id, name) VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie');",
    )?;
    txn.execute_statement(
        "INSERT INTO orders (id, customer_id, amount) VALUES (10, 1, 150), (20, 1, 200), (30, 2, 50), (40, 3, 300);",
    )?;

    // 1. Derived table
    let res_derived = txn.execute_sql(
        "SELECT o.id, o.customer_id, c.name FROM (SELECT id, customer_id FROM orders WHERE amount > 100) o JOIN customers c ON o.customer_id = c.id ORDER BY o.id ASC;",
    )?;
    assert_eq!(res_derived.rows.len(), 3); // orders 10 (Alice), 20 (Alice), 40 (Charlie)
    assert_eq!(res_derived.rows[0].values[0], Value::Int64(10));
    assert_eq!(
        res_derived.rows[0].values[2],
        Value::String("Alice".to_string())
    );
    assert_eq!(res_derived.rows[1].values[0], Value::Int64(20));
    assert_eq!(res_derived.rows[2].values[0], Value::Int64(40));

    // 2. EXISTS
    let res_exists = txn.execute_sql(
        "SELECT c.id, c.name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id) ORDER BY c.id ASC;",
    )?;
    assert_eq!(res_exists.rows.len(), 3); // all 3 customers have orders

    // 3. NOT EXISTS (with filter ensuring some don't match)
    let res_not_exists = txn.execute_sql(
        "SELECT c.id, c.name FROM customers c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 250) ORDER BY c.id ASC;",
    )?;
    // Charlie has order 40 with amount 300 (> 250). Alice (max 200) and Bob (max 50) have no order > 250.
    assert_eq!(res_not_exists.rows.len(), 2);
    assert_eq!(
        res_not_exists.rows[0].values[1],
        Value::String("Alice".to_string())
    );
    assert_eq!(
        res_not_exists.rows[1].values[1],
        Value::String("Bob".to_string())
    );

    // 4. IN
    let res_in = txn.execute_sql(
        "SELECT c.id, c.name FROM customers c WHERE c.id IN (SELECT customer_id FROM orders WHERE amount <= 50) ORDER BY c.id ASC;",
    )?;
    assert_eq!(res_in.rows.len(), 1);
    assert_eq!(res_in.rows[0].values[1], Value::String("Bob".to_string()));

    // 5. NULL-sensitive NOT IN
    txn.execute_statement("CREATE TABLE t_outer (x BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE t_inner_with_null (id BIGINT PRIMARY KEY, y BIGINT);")?;
    txn.execute_statement("CREATE TABLE t_inner_empty (id BIGINT PRIMARY KEY, y BIGINT);")?;

    txn.execute_statement("INSERT INTO t_outer (x) VALUES (1), (2), (3);")?;
    txn.execute_statement("INSERT INTO t_inner_with_null (id, y) VALUES (1, 1), (2, NULL);")?;

    // 5a. Empty inner relation: ALL outer rows return
    let res_empty = txn.execute_sql(
        "SELECT x FROM t_outer WHERE x NOT IN (SELECT y FROM t_inner_empty) ORDER BY x ASC;",
    )?;
    assert_eq!(res_empty.rows.len(), 3);

    // 5b. Inner relation containing NULL: 3VL yields UNKNOWN, so 0 rows return
    let res_null =
        txn.execute_sql("SELECT x FROM t_outer WHERE x NOT IN (SELECT y FROM t_inner_with_null);")?;
    assert_eq!(res_null.rows.len(), 0);

    txn.commit()?;
    Ok(())
}
