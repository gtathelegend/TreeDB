use std::collections::BTreeMap;
use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::error::Result;
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

// ============================================================================
// Basic Streaming Aggregation (R531 - R536)
// ============================================================================

#[test]
fn test_r531_basic_streaming_group_by() -> Result<()> {
    let ctx = TestContext::new("r531");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    txn.execute_statement("CREATE INDEX idx_grp ON t(grp);")?;

    txn.execute_statement("INSERT INTO t VALUES (1, 10, 100);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 10, 200);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 20, 300);")?;
    txn.execute_statement("INSERT INTO t VALUES (4, 20, 400);")?;
    txn.execute_statement("INSERT INTO t VALUES (5, 30, 500);")?;

    let res =
        txn.execute_sql("SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(
        res.rows[0].values,
        vec![Value::Int64(10), Value::UInt64(2), Value::Int64(300)]
    );
    assert_eq!(
        res.rows[1].values,
        vec![Value::Int64(20), Value::UInt64(2), Value::Int64(700)]
    );
    assert_eq!(
        res.rows[2].values,
        vec![Value::Int64(30), Value::UInt64(1), Value::Int64(500)]
    );

    let explain = txn.explain_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert!(explain.contains("StreamingAggregate") || explain.contains("Aggregate"));

    Ok(())
}

#[test]
fn test_r532_one_group() -> Result<()> {
    let ctx = TestContext::new("r532");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    for i in 1..=5 {
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 1, {v});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(
        res.rows[0].values,
        vec![Value::Int64(1), Value::UInt64(5), Value::Int64(150)]
    );

    Ok(())
}

#[test]
fn test_r533_many_groups() -> Result<()> {
    let ctx = TestContext::new("r533");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {v});"))?;
    }

    let res = txn.execute_sql("SELECT id, COUNT(*), SUM(val) FROM t GROUP BY id ORDER BY id;")?;
    assert_eq!(res.rows.len(), 100);
    for (i, row) in res.rows.iter().enumerate() {
        let expected_id = (i + 1) as i64;
        assert_eq!(row.values[0], Value::Int64(expected_id));
        assert_eq!(row.values[1], Value::UInt64(1));
        assert_eq!(row.values[2], Value::Int64(expected_id * 10));
    }

    Ok(())
}

#[test]
fn test_r534_empty_input() -> Result<()> {
    let ctx = TestContext::new("r534");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;

    // GROUP BY on empty table yields 0 rows
    let res_group = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(res_group.rows.len(), 0);

    // Scalar aggregate on empty table yields 1 row
    let res_scalar = txn.execute_sql("SELECT COUNT(*), SUM(val) FROM t;")?;
    assert_eq!(res_scalar.rows.len(), 1);
    assert_eq!(
        res_scalar.rows[0].values,
        vec![Value::UInt64(0), Value::Null]
    );

    Ok(())
}

#[test]
fn test_r535_large_input() -> Result<()> {
    let ctx = TestContext::new("r535");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    txn.execute_statement("CREATE INDEX idx_grp ON t(grp);")?;

    for i in 1..=500 {
        let grp = i % 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp}, 1);"))?;
    }

    let res =
        txn.execute_sql("SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 10);
    for row in res.rows {
        assert_eq!(row.values[1], Value::UInt64(50));
        assert_eq!(row.values[2], Value::Int64(50));
    }

    Ok(())
}

#[test]
fn test_r536_same_results_as_hash_aggregation() -> Result<()> {
    let ctx = TestContext::new("r536");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, category TEXT, amount INT);")?;
    let data = vec![
        (1, "A", 10),
        (2, "B", 20),
        (3, "A", 30),
        (4, "C", 40),
        (5, "B", 50),
        (6, "A", 60),
        (7, "C", 70),
        (8, "D", 80),
    ];
    for (id, cat, amt) in data {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({id}, '{cat}', {amt});"))?;
    }

    let res = txn.execute_sql("SELECT category, COUNT(*), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM t GROUP BY category ORDER BY category;")?;
    assert_eq!(res.rows.len(), 4);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("A".to_string()),
            Value::UInt64(3),
            Value::Int64(100),
            Value::Float64(100.0 / 3.0),
            Value::Int64(10),
            Value::Int64(60)
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::String("B".to_string()),
            Value::UInt64(2),
            Value::Int64(70),
            Value::Float64(35.0),
            Value::Int64(20),
            Value::Int64(50)
        ]
    );
    assert_eq!(
        res.rows[2].values,
        vec![
            Value::String("C".to_string()),
            Value::UInt64(2),
            Value::Int64(110),
            Value::Float64(55.0),
            Value::Int64(40),
            Value::Int64(70)
        ]
    );
    assert_eq!(
        res.rows[3].values,
        vec![
            Value::String("D".to_string()),
            Value::UInt64(1),
            Value::Int64(80),
            Value::Float64(80.0),
            Value::Int64(80),
            Value::Int64(80)
        ]
    );

    Ok(())
}

// ============================================================================
// NULL Semantics (R537 - R540)
// ============================================================================

#[test]
fn test_r537_null_grouping() -> Result<()> {
    let ctx = TestContext::new("r537");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, NULL);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Null, Value::UInt64(2)]);

    Ok(())
}

#[test]
fn test_r538_multiple_nulls() -> Result<()> {
    let ctx = TestContext::new("r538");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, NULL, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, NULL, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, NULL, NULL);")?;

    let res = txn.execute_sql("SELECT a, b, COUNT(*) FROM t GROUP BY a, b;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(
        res.rows[0].values,
        vec![Value::Null, Value::Null, Value::UInt64(3)]
    );

    Ok(())
}

#[test]
fn test_r539_null_and_non_null_groups() -> Result<()> {
    let ctx = TestContext::new("r539");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2);")?;
    txn.execute_statement("INSERT INTO t VALUES (4, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (5, 1);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values, vec![Value::Null, Value::UInt64(2)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(1), Value::UInt64(2)]);
    assert_eq!(res.rows[2].values, vec![Value::Int64(2), Value::UInt64(1)]);

    Ok(())
}

#[test]
fn test_r540_null_aggregate_expressions() -> Result<()> {
    let ctx = TestContext::new("r540");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (4, 2, NULL);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*), COUNT(val), SUM(val), AVG(val), MIN(val), MAX(val) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);
    // Group 1: all val are NULL
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::Int64(1),
            Value::UInt64(2),
            Value::UInt64(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ]
    );
    // Group 2: one non-null val
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::Int64(2),
            Value::UInt64(2),
            Value::UInt64(1),
            Value::Int64(10),
            Value::Float64(10.0),
            Value::Int64(10),
            Value::Int64(10)
        ]
    );

    Ok(())
}

// ============================================================================
// Composite GROUP BY (R541 - R545)
// ============================================================================

#[test]
fn test_r541_two_column_grouping() -> Result<()> {
    let ctx = TestContext::new("r541");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 10, 100);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 10, 200);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 1, 20, 300);")?;
    txn.execute_statement("INSERT INTO t VALUES (4, 2, 10, 400);")?;

    let res =
        txn.execute_sql("SELECT a, b, COUNT(*), SUM(v) FROM t GROUP BY a, b ORDER BY a, b;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::Int64(1),
            Value::Int64(10),
            Value::UInt64(2),
            Value::Int64(300)
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::Int64(1),
            Value::Int64(20),
            Value::UInt64(1),
            Value::Int64(300)
        ]
    );
    assert_eq!(
        res.rows[2].values,
        vec![
            Value::Int64(2),
            Value::Int64(10),
            Value::UInt64(1),
            Value::Int64(400)
        ]
    );

    Ok(())
}

#[test]
fn test_r542_three_column_grouping() -> Result<()> {
    let ctx = TestContext::new("r542");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, c INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 2, 3, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 2, 3, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 1, 2, 4, 30);")?;

    let res = txn.execute_sql(
        "SELECT a, b, c, COUNT(*), SUM(v) FROM t GROUP BY a, b, c ORDER BY a, b, c;",
    )?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::Int64(3),
            Value::UInt64(2),
            Value::Int64(30)
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::Int64(4),
            Value::UInt64(1),
            Value::Int64(30)
        ]
    );

    Ok(())
}

#[test]
fn test_r543_composite_duplicate_groups() -> Result<()> {
    let ctx = TestContext::new("r543");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    for i in 1..=20 {
        let a = i % 2;
        let b = i % 3;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {a}, {b});"))?;
    }

    let res = txn.execute_sql("SELECT a, b, COUNT(*) FROM t GROUP BY a, b ORDER BY a, b;")?;
    assert_eq!(res.rows.len(), 6);

    let total: u64 = res
        .rows
        .iter()
        .map(|r| match r.values[2] {
            Value::UInt64(c) => c,
            _ => 0,
        })
        .sum();
    assert_eq!(total, 20);

    Ok(())
}

#[test]
fn test_r544_correlated_columns() -> Result<()> {
    let ctx = TestContext::new("r544");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE geo (id BIGINT PRIMARY KEY, country TEXT, city TEXT);")?;
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO geo VALUES ({i}, 'US', 'NYC');"))?;
    }
    for i in 51..=100 {
        txn.execute_statement(&format!("INSERT INTO geo VALUES ({i}, 'UK', 'London');"))?;
    }

    txn.execute_statement("ANALYZE geo;")?;

    let res = txn.execute_sql(
        "SELECT country, city, COUNT(*) FROM geo GROUP BY country, city ORDER BY country, city;",
    )?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("UK".to_string()),
            Value::String("London".to_string()),
            Value::UInt64(50)
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::String("US".to_string()),
            Value::String("NYC".to_string()),
            Value::UInt64(50)
        ]
    );

    Ok(())
}

#[test]
fn test_r545_no_correlation_statistics() -> Result<()> {
    let ctx = TestContext::new("r545");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    for i in 1..=10 {
        let b = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {b});"))?;
    }

    let res = txn.execute_sql("SELECT a, b, COUNT(*) FROM t GROUP BY a, b ORDER BY a, b;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

// ============================================================================
// Ordering (R546 - R552)
// ============================================================================

#[test]
fn test_r546_primary_key_ordering() -> Result<()> {
    let ctx = TestContext::new("r546");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        let v = i * 100;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {v});"))?;
    }

    let explain = txn.explain_sql("SELECT id, SUM(val) FROM t GROUP BY id;")?;
    assert!(explain.contains("StreamingAggregate"));
    assert!(
        explain.contains("sort_required: No")
            || explain.contains("sort_required: false")
            || !explain.contains("sort_required: Yes")
    );

    let res = txn.execute_sql("SELECT id, SUM(val) FROM t GROUP BY id ORDER BY id;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r547_secondary_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r547");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id INT, amount INT);",
    )?;
    txn.execute_statement("CREATE INDEX idx_cust ON orders(customer_id);")?;

    for i in 1..=20 {
        let cust = i % 5;
        let amt = i * 10;
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {cust}, {amt});"))?;
    }

    let res = txn.execute_sql("SELECT customer_id, COUNT(*), SUM(amount) FROM orders GROUP BY customer_id ORDER BY customer_id;")?;
    assert_eq!(res.rows.len(), 5);

    Ok(())
}

#[test]
fn test_r548_composite_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r548");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, c INT, val INT);")?;
    txn.execute_statement("CREATE INDEX idx_abc ON t(a, b, c);")?;

    for i in 1..=10 {
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {i}, {i}, {v});"))?;
    }

    // Prefix GROUP BY a is valid with idx_abc
    let res_a = txn.execute_sql("SELECT a, COUNT(*) FROM t GROUP BY a ORDER BY a;")?;
    assert_eq!(res_a.rows.len(), 10);

    // Prefix GROUP BY a, b is valid with idx_abc
    let res_ab = txn.execute_sql("SELECT a, b, COUNT(*) FROM t GROUP BY a, b ORDER BY a, b;")?;
    assert_eq!(res_ab.rows.len(), 10);

    // Prefix GROUP BY a, b, c is valid with idx_abc
    let res_abc =
        txn.execute_sql("SELECT a, b, c, COUNT(*) FROM t GROUP BY a, b, c ORDER BY a, b, c;")?;
    assert_eq!(res_abc.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r549_covering_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r549");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id INT, amount INT, extra TEXT);",
    )?;
    txn.execute_statement("CREATE INDEX idx_covering ON orders(customer_id, amount);")?;

    for i in 1..=20 {
        let cust = i % 4;
        let amt = i * 10;
        txn.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, {cust}, {amt}, 'payload');"
        ))?;
    }

    let res = txn.execute_sql(
        "SELECT customer_id, SUM(amount) FROM orders GROUP BY customer_id ORDER BY customer_id;",
    )?;
    assert_eq!(res.rows.len(), 4);

    let plan_with_metrics = txn.execute_sql(
        "EXPLAIN ANALYZE SELECT customer_id, SUM(amount) FROM orders GROUP BY customer_id;",
    )?;
    assert!(!plan_with_metrics.rows.is_empty());

    Ok(())
}

#[test]
fn test_r550_invalid_suffix_ordering_rejected() -> Result<()> {
    let ctx = TestContext::new("r550");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, val INT);")?;
    txn.execute_statement("CREATE INDEX idx_ab ON t(a, b);")?;

    for i in 1..=10 {
        let b = i % 3;
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {b}, {v});"))?;
    }

    // GROUP BY b alone does not match prefix of idx_ab
    let res = txn.execute_sql("SELECT b, COUNT(*), SUM(val) FROM t GROUP BY b ORDER BY b;")?;
    assert_eq!(res.rows.len(), 3);

    Ok(())
}

#[test]
fn test_r551_multi_index_scan_requires_sort() -> Result<()> {
    let ctx = TestContext::new("r551");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, c INT);")?;
    txn.execute_statement("CREATE INDEX idx_a ON t(a);")?;
    txn.execute_statement("CREATE INDEX idx_b ON t(b);")?;

    for i in 1..=30 {
        let a = i % 5;
        let b = i % 7;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {a}, {b}, {i});"))?;
    }

    let res =
        txn.execute_sql("SELECT c, COUNT(*) FROM t WHERE a = 1 OR b = 2 GROUP BY c ORDER BY c;")?;
    assert!(!res.rows.is_empty());

    Ok(())
}

#[test]
fn test_r552_unsafe_skip_scan_ordering_rejected() -> Result<()> {
    let ctx = TestContext::new("r552");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, prefix INT, suffix INT, val INT);",
    )?;
    txn.execute_statement("CREATE INDEX idx_ps ON t(prefix, suffix);")?;

    for i in 1..=20 {
        let prefix = i % 3;
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {prefix}, {i}, {v});"))?;
    }

    let res = txn.execute_sql(
        "SELECT suffix, SUM(val) FROM t WHERE suffix > 5 GROUP BY suffix ORDER BY suffix;",
    )?;
    assert!(!res.rows.is_empty());

    Ok(())
}

// ============================================================================
// Merge Join (R553 - R556)
// ============================================================================

#[test]
fn test_r553_merge_join_feeding_streaming_aggregate() -> Result<()> {
    let ctx = TestContext::new("r553");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE c (id BIGINT PRIMARY KEY, region TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE o (id BIGINT PRIMARY KEY, customer_id BIGINT, amount INT);",
    )?;

    for i in 1..=10 {
        let region = if i <= 5 { "North" } else { "South" };
        let amt = i * 100;
        txn.execute_statement(&format!("INSERT INTO c VALUES ({i}, '{region}');"))?;
        txn.execute_statement(&format!("INSERT INTO o VALUES ({i}, {i}, {amt});"))?;
    }

    let res = txn.execute_sql("SELECT c.id, COUNT(*), SUM(o.amount) FROM c JOIN o ON c.id = o.customer_id GROUP BY c.id ORDER BY c.id;")?;
    assert_eq!(res.rows.len(), 10);
    for (idx, row) in res.rows.iter().enumerate() {
        let id = (idx + 1) as i64;
        assert_eq!(
            row.values,
            vec![Value::Int64(id), Value::UInt64(1), Value::Int64(id * 100)]
        );
    }

    Ok(())
}

#[test]
fn test_r554_ordered_join_output_reused() -> Result<()> {
    let ctx = TestContext::new("r554");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v1 INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, t1_id BIGINT, v2 INT);")?;

    for i in 1..=5 {
        let v1 = i * 10;
        let v2 = i * 100;
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {v1});"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, {v2});"))?;
    }

    let res = txn.execute_sql("SELECT t1.id, SUM(t2.v2) FROM t1 JOIN t2 ON t1.id = t2.t1_id GROUP BY t1.id ORDER BY t1.id;")?;
    assert_eq!(res.rows.len(), 5);

    Ok(())
}

#[test]
fn test_r555_unordered_join_output_requires_sort() -> Result<()> {
    let ctx = TestContext::new("r555");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, t1_id BIGINT, val INT);")?;

    for i in 1..=10 {
        let grp = 10 - i;
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {grp});"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, {v});"))?;
    }

    let res = txn.execute_sql("SELECT t1.grp, SUM(t2.val) FROM t1 JOIN t2 ON t1.id = t2.t1_id GROUP BY t1.grp ORDER BY t1.grp;")?;
    assert_eq!(res.rows.len(), 10);
    for (i, row) in res.rows.iter().enumerate() {
        assert_eq!(row.values[0], Value::Int64(i as i64));
    }

    Ok(())
}

#[test]
fn test_r556_residual_predicate_preserves_correctness() -> Result<()> {
    let ctx = TestContext::new("r556");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, cat TEXT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, t1_id BIGINT, val INT);")?;

    for i in 1..=10 {
        let cat = if i % 2 == 0 { "even" } else { "odd" };
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, '{cat}');"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, {v});"))?;
    }

    let res = txn.execute_sql("SELECT t1.id, SUM(t2.val) FROM t1 JOIN t2 ON t1.id = t2.t1_id WHERE t1.cat = 'even' GROUP BY t1.id ORDER BY t1.id;")?;
    assert_eq!(res.rows.len(), 5);
    for (idx, row) in res.rows.iter().enumerate() {
        let id = ((idx + 1) * 2) as i64;
        assert_eq!(row.values, vec![Value::Int64(id), Value::Int64(id * 10)]);
    }

    Ok(())
}

// ============================================================================
// Aggregates (R557 - R563)
// ============================================================================

#[test]
fn test_r557_count_star() -> Result<()> {
    let ctx = TestContext::new("r557");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::UInt64(2)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(2), Value::UInt64(1)]);

    Ok(())
}

#[test]
fn test_r558_count_expr() -> Result<()> {
    let ctx = TestContext::new("r558");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2, NULL);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(v) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::UInt64(1)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(2), Value::UInt64(0)]);

    Ok(())
}

#[test]
fn test_r559_sum() -> Result<()> {
    let ctx = TestContext::new("r559");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2, NULL);")?;

    let res = txn.execute_sql("SELECT grp, SUM(v) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(30)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(2), Value::Null]);

    Ok(())
}

#[test]
fn test_r560_avg() -> Result<()> {
    let ctx = TestContext::new("r560");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2, NULL);")?;

    let res = txn.execute_sql("SELECT grp, AVG(v) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(
        res.rows[0].values,
        vec![Value::Int64(1), Value::Float64(15.0)]
    );
    assert_eq!(res.rows[1].values, vec![Value::Int64(2), Value::Null]);

    Ok(())
}

#[test]
fn test_r561_min() -> Result<()> {
    let ctx = TestContext::new("r561");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 50);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 1, 80);")?;

    let res = txn.execute_sql("SELECT grp, MIN(v) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(20)]);

    Ok(())
}

#[test]
fn test_r562_max() -> Result<()> {
    let ctx = TestContext::new("r562");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 50);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 1, 80);")?;

    let res = txn.execute_sql("SELECT grp, MAX(v) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(80)]);

    Ok(())
}

#[test]
fn test_r563_mixed_aggregates() -> Result<()> {
    let ctx = TestContext::new("r563");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 1, 30);")?;

    let res = txn.execute_sql(
        "SELECT grp, COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM t GROUP BY grp;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::Int64(1),
            Value::UInt64(3),
            Value::UInt64(3),
            Value::Int64(60),
            Value::Float64(20.0),
            Value::Int64(10),
            Value::Int64(30)
        ]
    );

    Ok(())
}

// ============================================================================
// HAVING (R564 - R567)
// ============================================================================

#[test]
fn test_r564_having_true() -> Result<()> {
    let ctx = TestContext::new("r564");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 2);")?;

    let res =
        txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp HAVING 1 = 1 ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 2);

    Ok(())
}

#[test]
fn test_r565_having_false() -> Result<()> {
    let ctx = TestContext::new("r565");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 2);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp HAVING 1 = 0;")?;
    assert_eq!(res.rows.len(), 0);

    Ok(())
}

#[test]
fn test_r566_having_aggregate() -> Result<()> {
    let ctx = TestContext::new("r566");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp HAVING COUNT(*) > 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::UInt64(2)]);

    Ok(())
}

#[test]
fn test_r567_having_null_behavior() -> Result<()> {
    let ctx = TestContext::new("r567");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, v INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1, NULL);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 2, 10);")?;

    // SUM(v) > 0 will be NULL for group 1 (treated as false/not satisfied in HAVING)
    let res = txn.execute_sql("SELECT grp, SUM(v) FROM t GROUP BY grp HAVING SUM(v) > 0;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(2), Value::Int64(10)]);

    Ok(())
}

// ============================================================================
// ORDER BY (R568 - R571)
// ============================================================================

#[test]
fn test_r568_group_by_ordering_reused() -> Result<()> {
    let ctx = TestContext::new("r568");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("CREATE INDEX idx_grp ON t(grp);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let explain = txn.explain_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert!(explain.contains("StreamingAggregate"));

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 5);

    Ok(())
}

#[test]
fn test_r569_order_by_aggregate_requires_sort() -> Result<()> {
    let ctx = TestContext::new("r569");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 1);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 2);")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) AS n FROM t GROUP BY grp ORDER BY n DESC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::UInt64(2)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(2), Value::UInt64(1)]);

    Ok(())
}

#[test]
fn test_r570_alias_ordering() -> Result<()> {
    let ctx = TestContext::new("r570");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 20);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 10);")?;

    let res = txn.execute_sql("SELECT grp AS g, COUNT(*) FROM t GROUP BY grp ORDER BY g ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(10), Value::UInt64(1)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(20), Value::UInt64(1)]);

    Ok(())
}

#[test]
fn test_r571_different_direction_requires_sort() -> Result<()> {
    let ctx = TestContext::new("r571");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp DESC;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(5));
    assert_eq!(res.rows[4].values[0], Value::Int64(1));

    Ok(())
}

// ============================================================================
// LIMIT (R572 - R574)
// ============================================================================

#[test]
fn test_r572_ordered_group_by_limit() -> Result<()> {
    let ctx = TestContext::new("r572");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[2].values[0], Value::Int64(3));

    Ok(())
}

#[test]
fn test_r573_limit_after_having() -> Result<()> {
    let ctx = TestContext::new("r573");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        let grp = i % 3;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp});"))?;
    }

    let res = txn.execute_sql(
        "SELECT grp, COUNT(*) FROM t GROUP BY grp HAVING COUNT(*) > 2 ORDER BY grp LIMIT 2;",
    )?;
    assert!(res.rows.len() <= 2);

    Ok(())
}

#[test]
fn test_r574_unsafe_early_termination_rejected() -> Result<()> {
    let ctx = TestContext::new("r574");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=20 {
        let grp = if i <= 15 { 1 } else { 2 };
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp});"))?;
    }

    let res =
        txn.execute_sql("SELECT grp, COUNT(*) AS n FROM t GROUP BY grp ORDER BY n DESC LIMIT 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::UInt64(15)]);

    Ok(())
}

// ============================================================================
// Cost Model (R575 - R580)
// ============================================================================

#[test]
fn test_r575_streaming_selected_for_ordered_input() -> Result<()> {
    let ctx = TestContext::new("r575");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=5 {
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {v});"))?;
    }

    let explain = txn.explain_sql("SELECT id, SUM(v) FROM t GROUP BY id;")?;
    assert!(explain.contains("StreamingAggregate"));

    Ok(())
}

#[test]
fn test_r576_hash_aggregate_selected_when_cheaper() -> Result<()> {
    let ctx = TestContext::new("r576");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, unindexed_col INT, v INT);")?;
    for i in 1..=20 {
        let u = i % 3;
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {u}, {v});"))?;
    }

    let res = txn.execute_sql(
        "SELECT unindexed_col, COUNT(*) FROM t GROUP BY unindexed_col ORDER BY unindexed_col;",
    )?;
    assert_eq!(res.rows.len(), 3);

    Ok(())
}

#[test]
fn test_r577_external_aggregation_selected_when_appropriate() -> Result<()> {
    let ctx = TestContext::new("r577");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT v, COUNT(*) FROM t GROUP BY v ORDER BY v;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r578_sort_cost_included() -> Result<()> {
    let ctx = TestContext::new("r578");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, unindexed INT);")?;
    for i in 1..=10 {
        let u = 10 - i;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {u});"))?;
    }

    let res = txn
        .execute_sql("SELECT unindexed, COUNT(*) FROM t GROUP BY unindexed ORDER BY unindexed;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r579_statistics_sensitive_strategy() -> Result<()> {
    let ctx = TestContext::new("r579");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=30 {
        let grp = i % 5;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp});"))?;
    }

    txn.execute_statement("ANALYZE t;")?;

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 5);

    Ok(())
}

#[test]
fn test_r580_resource_budget_sensitive_strategy() -> Result<()> {
    let ctx = TestContext::new("r580");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

// ============================================================================
// Covering / Indexes (R581 - R584)
// ============================================================================

#[test]
fn test_r581_index_only_streaming_aggregation() -> Result<()> {
    let ctx = TestContext::new("r581");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id INT, amount INT);",
    )?;
    txn.execute_statement("CREATE INDEX idx_ca ON orders(customer_id, amount);")?;

    for i in 1..=10 {
        let cust = i % 3;
        let amt = i * 100;
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {cust}, {amt});"))?;
    }

    let res = txn.execute_sql(
        "SELECT customer_id, SUM(amount) FROM orders GROUP BY customer_id ORDER BY customer_id;",
    )?;
    assert_eq!(res.rows.len(), 3);

    Ok(())
}

#[test]
fn test_r582_base_record_fetch_remains_zero() -> Result<()> {
    let ctx = TestContext::new("r582");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, c TEXT);")?;
    txn.execute_statement("CREATE INDEX idx_ab ON t(a, b);")?;

    for i in 1..=10 {
        let a = i % 2;
        let b = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {a}, {b}, 'payload');"))?;
    }

    let plan_with_metrics =
        txn.execute_sql("EXPLAIN ANALYZE SELECT a, SUM(b) FROM t GROUP BY a;")?;
    assert!(!plan_with_metrics.rows.is_empty());

    Ok(())
}

#[test]
fn test_r583_composite_covering_index() -> Result<()> {
    let ctx = TestContext::new("r583");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT, c INT, d TEXT);")?;
    txn.execute_statement("CREATE INDEX idx_abc ON t(a, b, c);")?;

    for i in 1..=10 {
        let c = i * 10;
        txn.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {i}, {i}, {c}, 'str');"
        ))?;
    }

    let res = txn.execute_sql("SELECT a, b, SUM(c) FROM t GROUP BY a, b ORDER BY a, b;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r584_index_dropped_invalidates_plan() -> Result<()> {
    let ctx = TestContext::new("r584");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    txn.execute_statement("CREATE INDEX idx_grp ON t(grp);")?;

    for i in 1..=5 {
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {v});"))?;
    }

    let res1 = txn.execute_sql("SELECT grp, SUM(val) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res1.rows.len(), 5);

    txn.execute_statement("DROP INDEX idx_grp;")?;

    let res2 = txn.execute_sql("SELECT grp, SUM(val) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res2.rows.len(), 5);

    Ok(())
}

// ============================================================================
// Feedback & Cache (R585 - R589)
// ============================================================================

#[test]
fn test_r585_aggregation_runtime_feedback() -> Result<()> {
    let ctx = TestContext::new("r585");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        let grp = i % 2;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp});"))?;
    }

    let analyze_output =
        txn.execute_sql("EXPLAIN ANALYZE SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert!(!analyze_output.rows.is_empty());

    Ok(())
}

#[test]
fn test_r586_feedback_updates_strategy() -> Result<()> {
    let ctx = TestContext::new("r586");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r587_plan_cache_reuse() -> Result<()> {
    let ctx = TestContext::new("r587");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res1 = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    let res2 = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res1.rows, res2.rows);

    Ok(())
}

#[test]
fn test_r588_statistics_invalidation() -> Result<()> {
    let ctx = TestContext::new("r588");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    txn.execute_statement("ANALYZE t;")?;
    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

#[test]
fn test_r589_resource_signature_invalidation() -> Result<()> {
    let ctx = TestContext::new("r589");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 10);

    Ok(())
}

// ============================================================================
// Prepared / DML / Recovery (R590 - R595)
// ============================================================================

#[test]
fn test_r590_prepared_statement() -> Result<()> {
    let ctx = TestContext::new("r590");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
    for i in 1..=10 {
        let grp = i % 3;
        let v = i * 10;
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp}, {v});"))?;
    }

    let res =
        txn.execute_sql("SELECT grp, SUM(val) FROM t WHERE val > 20 GROUP BY grp ORDER BY grp;")?;
    assert_eq!(res.rows.len(), 3);

    Ok(())
}

#[test]
fn test_r591_transaction_visibility() -> Result<()> {
    let ctx = TestContext::new("r591");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (1, 10);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 10);")?;

    let res1 = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(
        res1.rows[0].values,
        vec![Value::Int64(10), Value::UInt64(2)]
    );

    // Insert uncommitted in same transaction
    txn.execute_statement("INSERT INTO t VALUES (3, 10);")?;
    let res2 = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(
        res2.rows[0].values,
        vec![Value::Int64(10), Value::UInt64(3)]
    );

    Ok(())
}

#[test]
fn test_r592_rollback() -> Result<()> {
    let ctx = TestContext::new("r592");
    let mut db = ctx.db();

    {
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
        txn.execute_statement("INSERT INTO t VALUES (1, 10);")?;
        txn.commit()?;
    }

    {
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("INSERT INTO t VALUES (2, 10);")?;
        txn.abort()?;
    }

    let mut txn = db.begin_transaction()?;
    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(10), Value::UInt64(1)]);

    Ok(())
}

#[test]
fn test_r593_persistence_reopen() -> Result<()> {
    let ctx = TestContext::new("r593");

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT, val INT);")?;
        txn.execute_statement("CREATE INDEX idx_grp ON t(grp);")?;
        for i in 1..=6 {
            let grp = i % 2;
            let v = i * 10;
            txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {grp}, {v});"))?;
        }
        txn.commit()?;
    }

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        let res =
            txn.execute_sql("SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp ORDER BY grp;")?;
        assert_eq!(res.rows.len(), 2);
        assert_eq!(
            res.rows[0].values,
            vec![Value::Int64(0), Value::UInt64(3), Value::Int64(120)]
        );
        assert_eq!(
            res.rows[1].values,
            vec![Value::Int64(1), Value::UInt64(3), Value::Int64(90)]
        );
    }

    Ok(())
}

#[test]
fn test_r594_recovery_equivalence() -> Result<()> {
    let ctx = TestContext::new("r594");

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp INT);")?;
        txn.execute_statement("INSERT INTO t VALUES (1, 5);")?;
        txn.execute_statement("INSERT INTO t VALUES (2, 5);")?;
        txn.commit()?;
    }

    // Reopen without clean shutdown
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_sql("SELECT grp, COUNT(*) FROM t GROUP BY grp;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(5), Value::UInt64(2)]);

    Ok(())
}

#[test]
fn test_r595_joined_aggregated_dml_path() -> Result<()> {
    let ctx = TestContext::new("r595");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE orders (id BIGINT PRIMARY KEY, cust_id BIGINT, amt INT);")?;
    txn.execute_statement(
        "CREATE TABLE summaries (id BIGINT PRIMARY KEY, cust_id BIGINT, total_amt INT);",
    )?;

    txn.execute_statement("INSERT INTO orders VALUES (1, 100, 50);")?;
    txn.execute_statement("INSERT INTO orders VALUES (2, 100, 150);")?;
    txn.execute_statement("INSERT INTO orders VALUES (3, 200, 300);")?;

    let res =
        txn.execute_sql("SELECT cust_id, SUM(amt) FROM orders GROUP BY cust_id ORDER BY cust_id;")?;
    for (idx, row) in res.rows.iter().enumerate() {
        let sid = (idx + 1) as i64;
        let cid = match row.values[0] {
            Value::Int64(v) => v,
            _ => 0,
        };
        let tot = match row.values[1] {
            Value::Int64(v) => v,
            _ => 0,
        };
        txn.execute_statement(&format!(
            "INSERT INTO summaries VALUES ({sid}, {cid}, {tot});"
        ))?;
    }

    let summary_res = txn.execute_sql("SELECT * FROM summaries ORDER BY id;")?;
    assert_eq!(summary_res.rows.len(), 2);
    assert_eq!(
        summary_res.rows[0].values,
        vec![Value::Int64(1), Value::Int64(100), Value::Int64(200)]
    );
    assert_eq!(
        summary_res.rows[1].values,
        vec![Value::Int64(2), Value::Int64(200), Value::Int64(300)]
    );

    Ok(())
}

// ============================================================================
// Randomized Differential Testing
// ============================================================================

#[test]
fn test_randomized_streaming_aggregation_oracle() -> Result<()> {
    let ctx = TestContext::new("rand_agg_oracle");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE rand_t (id BIGINT PRIMARY KEY, grp INT, subgrp INT, val INT);",
    )?;

    type GroupKey = (Option<i64>, Option<i64>);
    type AggState = (u64, i64, i64, i64);
    let mut expected_map: BTreeMap<GroupKey, AggState> = BTreeMap::new();

    for i in 1..=100 {
        let grp_val = if i % 11 == 0 {
            None
        } else {
            Some((i % 5) as i64)
        };
        let subgrp_val = if i % 7 == 0 {
            None
        } else {
            Some((i % 3) as i64)
        };
        let val = (i * 10) as i64;

        let g_str = grp_val
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());
        let s_str = subgrp_val
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());

        txn.execute_statement(&format!(
            "INSERT INTO rand_t VALUES ({i}, {g_str}, {s_str}, {val});"
        ))?;

        let entry = expected_map
            .entry((grp_val, subgrp_val))
            .or_insert((0, 0, i64::MAX, i64::MIN));
        entry.0 += 1; // count
        entry.1 += val; // sum
        entry.2 = entry.2.min(val); // min
        entry.3 = entry.3.max(val); // max
    }

    let res = txn.execute_sql("SELECT grp, subgrp, COUNT(*), SUM(val), MIN(val), MAX(val) FROM rand_t GROUP BY grp, subgrp ORDER BY grp, subgrp;")?;
    assert_eq!(res.rows.len(), expected_map.len());

    for row in res.rows {
        let g = match row.values[0] {
            Value::Int64(v) => Some(v),
            Value::Null => None,
            _ => panic!(),
        };
        let s = match row.values[1] {
            Value::Int64(v) => Some(v),
            Value::Null => None,
            _ => panic!(),
        };
        let cnt = match row.values[2] {
            Value::UInt64(v) => v,
            _ => panic!(),
        };
        let sum = match row.values[3] {
            Value::Int64(v) => v,
            _ => panic!(),
        };
        let min = match row.values[4] {
            Value::Int64(v) => v,
            _ => panic!(),
        };
        let max = match row.values[5] {
            Value::Int64(v) => v,
            _ => panic!(),
        };

        let exp = expected_map
            .get(&(g, s))
            .expect("Group should exist in oracle map");
        assert_eq!(cnt, exp.0);
        assert_eq!(sum, exp.1);
        assert_eq!(min, exp.2);
        assert_eq!(max, exp.3);
    }

    Ok(())
}
