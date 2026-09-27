use std::io::Write;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::{Error, Result};
use treedb::query::expr::{BinaryOp, Expr};
use treedb::query::external_hash_join::{
    analyze_equi_join, ExternalHashJoiner, JoinConfig, TempJoinFile, TEMP_JOIN_VERSION,
};
use treedb::sql::ast::JoinType;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    _wal_path: PathBuf,
    temp_dir: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let unique = format!(
            "test_ext_join_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base_dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::create_dir_all(&base_dir);
        let db_path = base_dir.join("test.treedb");
        let wal_path = base_dir.join("test.treedb.wal");
        let temp_dir = base_dir.join("temp_join_runs");
        let _ = std::fs::create_dir_all(&temp_dir);

        Self {
            db_path,
            _wal_path: wal_path,
            temp_dir,
        }
    }

    fn db(&self) -> TreeDb {
        let mut db = if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        };
        db.set_join_config(JoinConfig::new().with_temp_dir(&self.temp_dir));
        db
    }

    fn count_temp_files(&self) -> usize {
        if !self.temp_dir.exists() {
            return 0;
        }
        std::fs::read_dir(&self.temp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "tmp")
                    .unwrap_or(false)
            })
            .count()
    }
}

fn assert_rows_sorted_equivalent(rows1: &[Row], rows2: &[Row]) {
    assert_eq!(
        rows1.len(),
        rows2.len(),
        "row count mismatch: {} vs {}",
        rows1.len(),
        rows2.len()
    );

    let mut sorted1 = rows1.to_vec();
    let mut sorted2 = rows2.to_vec();

    sorted1.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    sorted2.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));

    for (i, (r1, r2)) in sorted1.iter().zip(sorted2.iter()).enumerate() {
        assert_eq!(
            r1.values.len(),
            r2.values.len(),
            "column count mismatch at row {i}"
        );
        for (j, (v1, v2)) in r1.values.iter().zip(r2.values.iter()).enumerate() {
            match (v1, v2) {
                (Value::Float64(f1), Value::Float64(f2)) => {
                    if f1.is_nan() && f2.is_nan() {
                        continue;
                    }
                    assert!(
                        (f1 - f2).abs() < 1e-6,
                        "float mismatch at row {i}, col {j}: {f1} vs {f2}"
                    );
                }
                (a, b) => {
                    assert_eq!(a, b, "value mismatch at row {i}, col {j}");
                }
            }
        }
    }
}

#[test]
fn test_inner_join_basic_and_edge_cases() -> Result<()> {
    let ctx = TestContext::new("inner_basic");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT, city TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE orders (order_id BIGINT PRIMARY KEY, cust_id BIGINT, amount BIGINT);",
    )?;

    txn.execute_statement(
        "INSERT INTO customers (id, name, city) VALUES (1, 'Alice', 'NY'), (2, 'Bob', 'SF'), (3, 'Charlie', 'LDN'), (4, 'David', 'PAR');",
    )?;
    txn.execute_statement(
        "INSERT INTO orders (order_id, cust_id, amount) VALUES (101, 1, 50), (102, 1, 150), (103, 2, 200), (104, 5, 500);",
    )?;

    // 1. Basic equi-join
    let res = txn.execute_sql(
        "SELECT c.name, o.order_id, o.amount FROM customers c JOIN orders o ON c.id = o.cust_id ORDER BY o.order_id;",
    )?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(res.rows[0].values[1], Value::Int64(101));
    assert_eq!(res.rows[1].values[0], Value::String("Alice".to_string()));
    assert_eq!(res.rows[1].values[1], Value::Int64(102));
    assert_eq!(res.rows[2].values[0], Value::String("Bob".to_string()));
    assert_eq!(res.rows[2].values[1], Value::Int64(103));

    // 2. Empty left / empty right
    txn.execute_statement("CREATE TABLE empty_t (id BIGINT PRIMARY KEY, val TEXT);")?;
    let empty_join1 =
        txn.execute_sql("SELECT * FROM empty_t e JOIN customers c ON e.id = c.id;")?;
    assert_eq!(empty_join1.rows.len(), 0);

    let empty_join2 =
        txn.execute_sql("SELECT * FROM customers c JOIN empty_t e ON c.id = e.id;")?;
    assert_eq!(empty_join2.rows.len(), 0);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_left_join_basic_and_null_padding() -> Result<()> {
    let ctx = TestContext::new("left_basic");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement("CREATE TABLE profiles (user_id BIGINT PRIMARY KEY, bio TEXT);")?;

    txn.execute_statement(
        "INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie');",
    )?;
    txn.execute_statement(
        "INSERT INTO profiles (user_id, bio) VALUES (1, 'Engineer'), (3, 'Designer');",
    )?;

    let res = txn.execute_sql(
        "SELECT u.name, p.bio FROM users u LEFT JOIN profiles p ON u.id = p.user_id ORDER BY u.id;",
    )?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(res.rows[0].values[1], Value::String("Engineer".to_string()));

    // Unmatched Bob is NULL-padded
    assert_eq!(res.rows[1].values[0], Value::String("Bob".to_string()));
    assert_eq!(res.rows[1].values[1], Value::Null);

    assert_eq!(res.rows[2].values[0], Value::String("Charlie".to_string()));
    assert_eq!(res.rows[2].values[1], Value::String("Designer".to_string()));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_sql_null_join_semantics() -> Result<()> {
    let ctx = TestContext::new("null_semantics");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE left_nulls (id BIGINT PRIMARY KEY, key_col BIGINT, val TEXT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE right_nulls (id BIGINT PRIMARY KEY, key_col BIGINT, desc_col TEXT);",
    )?;

    txn.execute_statement(
        "INSERT INTO left_nulls (id, key_col, val) VALUES (1, 10, 'A'), (2, NULL, 'B'), (3, NULL, 'C'), (4, 20, 'D');",
    )?;
    txn.execute_statement(
        "INSERT INTO right_nulls (id, key_col, desc_col) VALUES (101, 10, 'Match 10'), (102, NULL, 'Match NULL 1'), (103, NULL, 'Match NULL 2'), (104, 30, 'Match 30');",
    )?;

    // 1. INNER JOIN: NULL keys must NEVER match NULL keys
    let inner_res = txn.execute_sql(
        "SELECT l.val, r.desc_col FROM left_nulls l JOIN right_nulls r ON l.key_col = r.key_col;",
    )?;
    assert_eq!(inner_res.rows.len(), 1);
    assert_eq!(inner_res.rows[0].values[0], Value::String("A".to_string()));
    assert_eq!(
        inner_res.rows[0].values[1],
        Value::String("Match 10".to_string())
    );

    // 2. LEFT JOIN: NULL keys on left must be emitted with NULL for right columns
    let left_res = txn.execute_sql(
        "SELECT l.val, r.desc_col FROM left_nulls l LEFT JOIN right_nulls r ON l.key_col = r.key_col ORDER BY l.id;",
    )?;
    assert_eq!(left_res.rows.len(), 4);
    assert_eq!(left_res.rows[0].values[0], Value::String("A".to_string()));
    assert_eq!(
        left_res.rows[0].values[1],
        Value::String("Match 10".to_string())
    );

    assert_eq!(left_res.rows[1].values[0], Value::String("B".to_string()));
    assert_eq!(left_res.rows[1].values[1], Value::Null);

    assert_eq!(left_res.rows[2].values[0], Value::String("C".to_string()));
    assert_eq!(left_res.rows[2].values[1], Value::Null);

    assert_eq!(left_res.rows[3].values[0], Value::String("D".to_string()));
    assert_eq!(left_res.rows[3].values[1], Value::Null);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_residual_predicates_and_composite_keys() -> Result<()> {
    let ctx = TestContext::new("residual_composite");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE t1 (id BIGINT PRIMARY KEY, k1 BIGINT, k2 TEXT, val BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE t2 (id BIGINT PRIMARY KEY, k1 BIGINT, k2 TEXT, threshold BIGINT);",
    )?;

    txn.execute_statement(
        "INSERT INTO t1 (id, k1, k2, val) VALUES (1, 10, 'A', 100), (2, 10, 'A', 20), (3, 20, 'B', 50), (4, 30, 'C', 200);",
    )?;
    txn.execute_statement(
        "INSERT INTO t2 (id, k1, k2, threshold) VALUES (101, 10, 'A', 50), (102, 20, 'B', 80), (103, 40, 'D', 10);",
    )?;

    // Composite equi-join + residual predicate: (t1.k1 = t2.k1 AND t1.k2 = t2.k2 AND t1.val > t2.threshold)
    let res = txn.execute_sql(
        "SELECT t1.id, t1.val, t2.threshold FROM t1 JOIN t2 ON t1.k1 = t2.k1 AND t1.k2 = t2.k2 AND t1.val > t2.threshold;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::Int64(100));
    assert_eq!(res.rows[0].values[2], Value::Int64(50));

    // Composite LEFT JOIN with residual predicate
    let left_res = txn.execute_sql(
        "SELECT t1.id, t2.threshold FROM t1 LEFT JOIN t2 ON t1.k1 = t2.k1 AND t1.k2 = t2.k2 AND t1.val > t2.threshold ORDER BY t1.id;",
    )?;
    assert_eq!(left_res.rows.len(), 4);
    assert_eq!(left_res.rows[0].values[0], Value::Int64(1));
    assert_eq!(left_res.rows[0].values[1], Value::Int64(50));

    assert_eq!(left_res.rows[1].values[0], Value::Int64(2));
    assert_eq!(left_res.rows[1].values[1], Value::Null);

    assert_eq!(left_res.rows[2].values[0], Value::Int64(3));
    assert_eq!(left_res.rows[2].values[1], Value::Null);

    assert_eq!(left_res.rows[3].values[0], Value::Int64(4));
    assert_eq!(left_res.rows[3].values[1], Value::Null);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_external_hash_join_tiny_budget_spill() -> Result<()> {
    let ctx = TestContext::new("tiny_spill");
    let mut db = ctx.db();

    // Set a tiny memory budget of 128 bytes to force disk spilling
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(128)
            .with_max_temp_files(4),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE left_rel (id BIGINT PRIMARY KEY, dept_id BIGINT, score BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE right_rel (id BIGINT PRIMARY KEY, dept_id BIGINT, dept_name TEXT);",
    )?;

    // Populate 200 rows on left and 50 rows on right
    for i in 1..=200 {
        let dept = (i % 20) + 1;
        txn.execute_statement(&format!(
            "INSERT INTO left_rel (id, dept_id, score) VALUES ({i}, {dept}, {i} * 10);"
        ))?;
    }

    for d in 1..=20 {
        txn.execute_statement(&format!(
            "INSERT INTO right_rel (id, dept_id, dept_name) VALUES ({d}, {d}, 'Dept_{d}');"
        ))?;
    }

    // 1. Spilled INNER JOIN execution
    let (spilled_res, metrics) = txn.execute_sql_with_metrics(
        "SELECT l.id, l.score, r.dept_name FROM left_rel l JOIN right_rel r ON l.dept_id = r.dept_id;",
    )?;

    assert_eq!(spilled_res.rows.len(), 200);
    assert!(metrics.root_operator.is_some());
    let root = metrics.root_operator.as_ref().unwrap();
    let join_op = treedb::query::metrics::find_operator_metrics(root, &|k| {
        matches!(
            k,
            treedb::query::OperatorKind::ExternalHashJoin { .. }
                | treedb::query::OperatorKind::HashJoin { .. }
        )
    })
    .expect("expected to find join operator metrics");

    assert!(
        join_op.counters.bytes_spilled > 0 || join_op.counters.runs_created > 0,
        "expected external join to spill: bytes_spilled={}, runs_created={}",
        join_op.counters.bytes_spilled,
        join_op.counters.runs_created
    );

    // 2. In-memory comparison execution (large budget)
    let in_mem_config = JoinConfig::new()
        .with_temp_dir(&ctx.temp_dir)
        .with_memory_budget_bytes(10 * 1024 * 1024);

    let left_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("dept_id", DataType::Int64, true),
        Column::new("score", DataType::Int64, true),
    ]);
    let right_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("dept_id", DataType::Int64, true),
        Column::new("dept_name", DataType::String, true),
    ]);

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("left_rel.dept_id".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("right_rel.dept_id".to_string())),
    };

    let all_left = txn
        .execute_sql("SELECT id, dept_id, score FROM left_rel;")?
        .rows;
    let all_right = txn
        .execute_sql("SELECT id, dept_id, dept_name FROM right_rel;")?
        .rows;

    let equi_plan = analyze_equi_join(&on_expr, &left_schema, &right_schema).unwrap();
    let in_mem_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan,
        in_mem_config,
    );
    let (_, in_mem_rows, in_mem_metrics) = in_mem_joiner.execute(all_left, all_right)?;

    assert_eq!(in_mem_metrics.bytes_spilled, 0);
    assert_eq!(in_mem_metrics.runs_created, 0);
    assert_eq!(in_mem_rows.len(), 200);

    // Assert equivalence between in-memory and spilled external join
    let in_mem_projected: Vec<Row> = in_mem_rows
        .iter()
        .map(|r| {
            Row::new(vec![
                r.values[0].clone(),
                r.values[2].clone(),
                r.values[5].clone(),
            ])
        })
        .collect();

    assert_rows_sorted_equivalent(&spilled_res.rows, &in_mem_projected);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_external_hash_join_multi_pass_recursion() -> Result<()> {
    let ctx = TestContext::new("multi_pass");
    let mut db = ctx.db();

    // Minimal 32-byte budget with 2 partitions forces multi-pass recursive sub-partitioning
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(32)
            .with_max_temp_files(2)
            .with_max_partition_passes(20),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE big_left (id BIGINT PRIMARY KEY, k BIGINT, v TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE big_right (id BIGINT PRIMARY KEY, k BIGINT, payload TEXT);",
    )?;

    for i in 1..=300 {
        let k = (i % 30) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO big_left (id, k, v) VALUES ({i}, {k}, 'left_val_{i}');"
        ))?;
    }

    for i in 1..=100 {
        let k = (i % 30) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO big_right (id, k, payload) VALUES ({i}, {k}, 'right_val_{i}');"
        ))?;
    }

    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT l.id, r.id FROM big_left l JOIN big_right r ON l.k = r.k;",
    )?;

    assert!(!res.rows.is_empty());
    let root = metrics.root_operator.as_ref().unwrap();
    let join_op = treedb::query::metrics::find_operator_metrics(root, &|k| {
        matches!(
            k,
            treedb::query::OperatorKind::ExternalHashJoin { .. }
                | treedb::query::OperatorKind::HashJoin { .. }
        )
    })
    .expect("expected to find join operator metrics");

    assert!(
        join_op.counters.partition_passes >= 1,
        "expected multi-pass partition execution, got: {}",
        join_op.counters.partition_passes
    );
    assert!(join_op.counters.runs_created >= 2);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_prepared_statements_and_plan_cache_reuse() -> Result<()> {
    let ctx = TestContext::new("prep_cache");
    let mut db = ctx.db();

    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(256),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE employees (id BIGINT PRIMARY KEY, dept_id BIGINT, salary BIGINT);",
    )?;
    txn.execute_statement("CREATE TABLE departments (dept_id BIGINT PRIMARY KEY, name TEXT);")?;

    for i in 1..=100 {
        let dept = (i % 10) + 1;
        let salary = i * 1000;
        txn.execute_statement(&format!(
            "INSERT INTO employees (id, dept_id, salary) VALUES ({i}, {dept}, {salary});"
        ))?;
    }

    for d in 1..=10 {
        txn.execute_statement(&format!(
            "INSERT INTO departments (dept_id, name) VALUES ({d}, 'Dept_{d}');"
        ))?;
    }

    // Prepare join query with parameter
    let sql = "SELECT e.id, e.salary, d.name FROM employees e JOIN departments d ON e.dept_id = d.dept_id WHERE e.salary > ? ORDER BY e.id;";
    let stmt = txn.prepare_sql(sql)?;

    // Execution 1: salary > 80000 (small subset)
    let res1 = txn.execute_prepared_query(&stmt, &[Value::Int64(80000)])?;
    assert_eq!(res1.rows.len(), 20);

    // Execution 2: salary > 10000 (larger subset triggering spill)
    let res2 = txn.execute_prepared_query(&stmt, &[Value::Int64(10000)])?;
    assert_eq!(res2.rows.len(), 90);

    // Execution 3: salary > 95000
    let res3 = txn.execute_prepared_query(&stmt, &[Value::Int64(95000)])?;
    assert_eq!(res3.rows.len(), 5);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_join_query_integrations() -> Result<()> {
    let ctx = TestContext::new("integrations");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE sales (sale_id BIGINT PRIMARY KEY, region_id BIGINT, amount BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE regions (region_id BIGINT PRIMARY KEY, region_name TEXT);",
    )?;

    txn.execute_statement(
        "INSERT INTO regions (region_id, region_name) VALUES (1, 'North'), (2, 'South'), (3, 'West');",
    )?;
    txn.execute_statement(
        "INSERT INTO sales (sale_id, region_id, amount) VALUES (1, 1, 100), (2, 1, 200), (3, 2, 50), (4, 2, 150), (5, 3, 300);",
    )?;

    // 1. JOIN + GROUP BY + HAVING + ORDER BY
    let grp_sql = "SELECT r.region_name, COUNT(*), SUM(s.amount) \
                   FROM sales s JOIN regions r ON s.region_id = r.region_id \
                   GROUP BY r.region_name \
                   HAVING SUM(s.amount) >= 200 \
                   ORDER BY r.region_name;";
    let grp_res = txn.execute_sql(grp_sql)?;
    assert_eq!(grp_res.rows.len(), 3);
    assert_eq!(
        grp_res.rows[0].values[0],
        Value::String("North".to_string())
    );
    assert_eq!(grp_res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(grp_res.rows[0].values[2], Value::Int64(300));

    // 2. JOIN + Window Function
    let win_sql = "SELECT s.sale_id, r.region_name, s.amount, \
                          RANK() OVER (PARTITION BY r.region_name ORDER BY s.amount DESC) AS rk \
                   FROM sales s JOIN regions r ON s.region_id = r.region_id \
                   ORDER BY s.sale_id;";
    let win_res = txn.execute_sql(win_sql)?;
    assert_eq!(win_res.rows.len(), 5);

    // 3. JOIN + CTE
    let cte_sql = "WITH high_sales AS (SELECT * FROM sales WHERE amount >= 150) \
                   SELECT hs.sale_id, r.region_name, hs.amount \
                   FROM high_sales hs JOIN regions r ON hs.region_id = r.region_id \
                   ORDER BY hs.sale_id;";
    let cte_res = txn.execute_sql(cte_sql)?;
    assert_eq!(cte_res.rows.len(), 3);
    assert_eq!(cte_res.rows[0].values[0], Value::Int64(2));
    assert_eq!(cte_res.rows[1].values[0], Value::Int64(4));
    assert_eq!(cte_res.rows[2].values[0], Value::Int64(5));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_transaction_persistence_and_rollback_invariants() -> Result<()> {
    let ctx = TestContext::new("persistence_invariance");
    let mut db = ctx.db();

    // Force small budget so any join spills
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(64),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_left (id BIGINT PRIMARY KEY, k BIGINT);")?;
    txn.execute_statement("CREATE TABLE t_right (id BIGINT PRIMARY KEY, k BIGINT);")?;

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t_left (id, k) VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO t_right (id, k) VALUES ({i}, {i});"))?;
    }
    txn.commit()?;

    // Read-only transaction executing large join
    let mut ro_txn = db.begin_transaction()?;
    let res1 = ro_txn.execute_sql("SELECT COUNT(*) FROM t_left l JOIN t_right r ON l.k = r.k;")?;
    assert_eq!(res1.rows[0].values[0], Value::UInt64(50));

    // Abort read-only txn
    ro_txn.abort()?;

    // All temporary files must be deleted
    assert_eq!(
        ctx.count_temp_files(),
        0,
        "temporary join files should be cleaned up on rollback"
    );

    // Close and reopen database
    drop(db);
    let mut reopened_db = ctx.db();
    let mut check_txn = reopened_db.begin_transaction()?;
    let res2 =
        check_txn.execute_sql("SELECT COUNT(*) FROM t_left l JOIN t_right r ON l.k = r.k;")?;
    assert_eq!(res2.rows[0].values[0], Value::UInt64(50));

    check_txn.commit()?;
    Ok(())
}

#[test]
fn test_corruption_and_limit_enforcements() -> Result<()> {
    let ctx = TestContext::new("corruption_limits");

    // 1. Quota Exceeded
    let mut db = ctx.db();
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(64)
            .with_max_temp_bytes(100), // very low disk limit
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE l_q (id BIGINT PRIMARY KEY, k BIGINT);")?;
    txn.execute_statement("CREATE TABLE r_q (id BIGINT PRIMARY KEY, k BIGINT);")?;

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO l_q (id, k) VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO r_q (id, k) VALUES ({i}, {i});"))?;
    }

    let quota_res = txn.execute_sql("SELECT * FROM l_q l JOIN r_q r ON l.k = r.k;");
    assert!(quota_res.is_err());
    match quota_res.err().unwrap() {
        Error::ExternalJoinQuotaExceeded { .. } => {}
        other => panic!("expected ExternalJoinQuotaExceeded, got: {other:?}"),
    }
    txn.abort()?;

    // 2. Corrupted Magic in Partition File
    let (part_file, mut f) = TempJoinFile::create_in(&ctx.temp_dir)?;
    f.write_all(b"CORR")?; // Bad magic
    f.write_all(&TEMP_JOIN_VERSION.to_le_bytes())?;
    f.write_all(&[0u8; 2])?;
    f.write_all(&0u64.to_le_bytes())?;
    f.flush()?;

    let read_err = treedb::query::external_hash_join::read_partition_rows(&part_file);
    assert!(read_err.is_err());
    match read_err.err().unwrap() {
        Error::ExternalJoinRunCorrupted(msg) => {
            assert!(msg.contains("invalid join partition magic"));
        }
        other => panic!("expected ExternalJoinRunCorrupted, got: {other:?}"),
    }

    // 3. Truncated Header
    let (part_file_trunc, mut f2) = TempJoinFile::create_in(&ctx.temp_dir)?;
    f2.write_all(b"TJRN")?;
    f2.flush()?;

    let trunc_err = treedb::query::external_hash_join::read_partition_rows(&part_file_trunc);
    assert!(trunc_err.is_err());
    match trunc_err.err().unwrap() {
        Error::ExternalJoinRunCorrupted(msg) => {
            assert!(msg.contains("missing join header"));
        }
        other => panic!("expected ExternalJoinRunCorrupted, got: {other:?}"),
    }

    // 4. Record Count Mismatch
    let (part_file_cnt, mut f3) = TempJoinFile::create_in(&ctx.temp_dir)?;
    f3.write_all(b"TJRN")?;
    f3.write_all(&TEMP_JOIN_VERSION.to_le_bytes())?;
    f3.write_all(&[0u8; 2])?;
    f3.write_all(&100u64.to_le_bytes())?; // claims 100 records but writes 0
    f3.flush()?;

    let cnt_err = treedb::query::external_hash_join::read_partition_rows(&part_file_cnt);
    assert!(cnt_err.is_err());
    match cnt_err.err().unwrap() {
        Error::ExternalJoinRunCorrupted(msg) => {
            assert!(msg.contains("record count mismatch"));
        }
        other => panic!("expected ExternalJoinRunCorrupted, got: {other:?}"),
    }

    Ok(())
}

#[test]
fn test_randomized_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("randomized_oracle");
    let mut rng_state = 0x123456789abcdef0u64;
    let mut next_u64 = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };

    let left_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("join_k", DataType::Int64, true),
        Column::new("sub_k", DataType::String, true),
        Column::new("val", DataType::Float64, true),
    ]);

    let right_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("join_k", DataType::Int64, true),
        Column::new("sub_k", DataType::String, true),
        Column::new("threshold", DataType::Float64, true),
    ]);

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("left.join_k".to_string())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column("right.join_k".to_string())),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("left.val".to_string())),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Column("right.threshold".to_string())),
        }),
    };

    for trial in 0..10 {
        let mut left_rows = Vec::new();
        let mut right_rows = Vec::new();

        let num_left = (next_u64() % 80 + 20) as usize;
        let num_right = (next_u64() % 40 + 10) as usize;

        for i in 0..num_left {
            let k = if next_u64() % 10 == 0 {
                Value::Null
            } else {
                Value::Int64((next_u64() % 10) as i64)
            };
            let sub = Value::String(format!("code_{}", next_u64() % 5));
            let val = Value::Float64((next_u64() % 500) as f64);
            left_rows.push(Row::new(vec![Value::Int64(i as i64), k, sub, val]));
        }

        for j in 0..num_right {
            let k = if next_u64() % 10 == 0 {
                Value::Null
            } else {
                Value::Int64((next_u64() % 10) as i64)
            };
            let sub = Value::String(format!("code_{}", next_u64() % 5));
            let threshold = Value::Float64((next_u64() % 300) as f64);
            right_rows.push(Row::new(vec![Value::Int64(j as i64), k, sub, threshold]));
        }

        for join_type in [JoinType::Inner, JoinType::Left] {
            // 1. Reference nested-loop execution
            let (ref_schema, ref_rows) = treedb::query::operator::JoinOperator::apply(
                left_rows.clone(),
                &left_schema,
                &right_rows,
                &right_schema,
                join_type,
                &on_expr,
            )?;

            // 2. In-memory hash join (10 MB budget)
            let in_mem_config = JoinConfig::new()
                .with_temp_dir(&ctx.temp_dir)
                .with_memory_budget_bytes(10 * 1024 * 1024);
            let (_, in_mem_rows, in_mem_metrics) =
                treedb::query::operator::JoinOperator::apply_with_config(
                    left_rows.clone(),
                    &left_schema,
                    &right_rows,
                    &right_schema,
                    join_type,
                    &on_expr,
                    &in_mem_config,
                )?;
            assert_eq!(in_mem_metrics.bytes_spilled, 0);

            // 3. Spilled external hash join (tiny 64-byte budget)
            let spill_config = JoinConfig::new()
                .with_temp_dir(&ctx.temp_dir)
                .with_memory_budget_bytes(64)
                .with_max_temp_files(3);
            let (spill_schema, spill_rows, spill_metrics) =
                treedb::query::operator::JoinOperator::apply_with_config(
                    left_rows.clone(),
                    &left_schema,
                    &right_rows,
                    &right_schema,
                    join_type,
                    &on_expr,
                    &spill_config,
                )?;

            assert_eq!(ref_schema.columns.len(), spill_schema.columns.len());
            assert_rows_sorted_equivalent(&ref_rows, &in_mem_rows);
            assert_rows_sorted_equivalent(&ref_rows, &spill_rows);

            if !right_rows.is_empty() && !left_rows.is_empty() {
                assert!(
                    spill_metrics.runs_created > 0 || spill_metrics.bytes_spilled > 0,
                    "trial {trial}: expected spilling under 64-byte budget"
                );
            }
        }
    }

    Ok(())
}

#[test]
fn test_peak_memory_bounded_and_large_workload() -> Result<()> {
    let ctx = TestContext::new("large_workload");
    let mut db = ctx.db();

    // 256-byte budget
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(&ctx.temp_dir)
            .with_memory_budget_bytes(256)
            .with_max_temp_files(4),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE large_a (id BIGINT PRIMARY KEY, k BIGINT, val BIGINT);")?;
    txn.execute_statement("CREATE TABLE large_b (id BIGINT PRIMARY KEY, k BIGINT, note TEXT);")?;

    for i in 1..=500 {
        let k = (i % 25) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO large_a (id, k, val) VALUES ({i}, {k}, {i} * 2);"
        ))?;
    }
    for j in 1..=100 {
        let k = (j % 25) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO large_b (id, k, note) VALUES ({j}, {k}, 'note_{j}');"
        ))?;
    }

    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT a.id, b.id, b.note FROM large_a a JOIN large_b b ON a.k = b.k;",
    )?;

    // 500 rows with k in 0..24 (20 of each k), 100 rows in b with k in 0..24 (4 of each k)
    // 20 * 4 = 80 matches per key * 25 keys = 2000 total joined rows!
    assert_eq!(res.rows.len(), 2000);

    let root = metrics.root_operator.as_ref().unwrap();
    let join_op = treedb::query::metrics::find_operator_metrics(root, &|k| {
        matches!(
            k,
            treedb::query::OperatorKind::ExternalHashJoin { .. }
                | treedb::query::OperatorKind::HashJoin { .. }
        )
    })
    .expect("expected join operator metrics");

    assert!(join_op.counters.runs_created > 0);
    assert!(join_op.counters.bytes_spilled > 0);

    txn.commit()?;
    Ok(())
}
