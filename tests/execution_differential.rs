use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Row, Value};
use treedb::error::Error;
use treedb::query::operator::CanonicalValue;
use treedb::query::reference::{assert_differential_equivalence, CanonicalRow};
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_exec_diff_{name}.treedb"));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        Self {
            _dir: dir,
            db_path,
            wal_path,
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

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

// -----------------------------------------------------------------------------
// 1. Canonical Row & Primitive Types Comparison (Section 5)
// -----------------------------------------------------------------------------

#[test]
fn test_canonical_row_primitives_and_nan_floats() {
    let r1 = Row::new(vec![
        Value::Null,
        Value::Boolean(true),
        Value::Int64(-42),
        Value::UInt64(100),
        Value::Float64(f64::NAN),
        Value::Float64(-0.0),
        Value::String("test".to_string()),
        Value::Bytes(vec![1, 2, 3]),
    ]);

    let r2 = Row::new(vec![
        Value::Null,
        Value::Boolean(true),
        Value::Int64(-42),
        Value::UInt64(100),
        Value::Float64(f64::NAN),
        Value::Float64(0.0), // -0.0 and +0.0 must canonicalize equally
        Value::String("test".to_string()),
        Value::Bytes(vec![1, 2, 3]),
    ]);

    let cr1 = CanonicalRow::from_row(&r1);
    let cr2 = CanonicalRow::from_row(&r2);
    assert_eq!(cr1, cr2, "CanonicalRow must equate NaNs and -0.0 vs +0.0");

    let mut rows = [
        CanonicalRow::from_row(&Row::new(vec![Value::Int64(10)])),
        CanonicalRow::from_row(&Row::new(vec![Value::Null])),
        CanonicalRow::from_row(&Row::new(vec![Value::Int64(5)])),
    ];
    rows.sort();
    assert_eq!(
        rows[0],
        CanonicalRow(vec![CanonicalValue::Null]),
        "Nulls sort first by default in CanonicalValue"
    );
}

// -----------------------------------------------------------------------------
// 2. Basic SELECT and Arithmetic Differential Equivalence (Sections 4, 8)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_select_basic_and_arithmetic() {
    let ctx = TestContext::new("select_basic");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE numbers (id UINT64 PRIMARY KEY, a INT64, b INT64, f FLOAT64)",
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO numbers VALUES (1, 10, 20, 2.5)")
        .unwrap();
    txn.execute_statement("INSERT INTO numbers VALUES (2, -15, 0, 0.0)")
        .unwrap();
    txn.execute_statement("INSERT INTO numbers VALUES (3, 100, NULL, 5.0)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        "SELECT a + b, a - b, a * b FROM numbers",
        "SELECT a / b FROM numbers", // division by zero and nulls
        "SELECT a, f, a + 10 FROM numbers WHERE a > 0",
        "SELECT a, b FROM numbers WHERE a < 0 OR b IS NULL",
        "SELECT id, a FROM numbers ORDER BY a ASC",
        "SELECT id, a FROM numbers ORDER BY a DESC LIMIT 2",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        let requires_order = sql.contains("ORDER BY");
        assert_differential_equivalence(&opt, &ref_res, requires_order);
    }
}

// -----------------------------------------------------------------------------
// 3. Three-Valued Logic (3VL) Stress Testing (Section 9)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_3vl_truth_tables() {
    let ctx = TestContext::new("3vl_truth_tables");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE tvl (id UINT64 PRIMARY KEY, val BOOLEAN)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO tvl VALUES (1, TRUE)")
        .unwrap();
    txn.execute_statement("INSERT INTO tvl VALUES (2, FALSE)")
        .unwrap();
    txn.execute_statement("INSERT INTO tvl VALUES (3, NULL)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // Truth table combinations
        "SELECT id, val FROM tvl WHERE val IS NULL",
        "SELECT id, val FROM tvl WHERE val IS NOT NULL",
        "SELECT id, val FROM tvl WHERE val = TRUE",
        "SELECT id, val FROM tvl WHERE val = FALSE",
        "SELECT id, val FROM tvl WHERE val <> TRUE",
        "SELECT id, val FROM tvl WHERE val <> FALSE",
        "SELECT id, val FROM tvl WHERE NOT (val = TRUE)",
        "SELECT id, val FROM tvl WHERE val AND TRUE",
        "SELECT id, val FROM tvl WHERE val AND FALSE",
        "SELECT id, val FROM tvl WHERE val OR TRUE",
        "SELECT id, val FROM tvl WHERE val OR FALSE",
        // Contradictory and always-true / always-false predicates
        "SELECT id FROM tvl WHERE val = TRUE AND val = FALSE",
        "SELECT id FROM tvl WHERE 1 = 1",
        "SELECT id FROM tvl WHERE 1 = 0",
        "SELECT id FROM tvl WHERE NULL = NULL",
        "SELECT id FROM tvl WHERE NULL <> NULL",
        "SELECT id FROM tvl WHERE NULL = 1",
        "SELECT id FROM tvl WHERE NOT (NULL = 1)",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 4. IN and NOT IN Predicate Stress (Section 10)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_in_and_not_in_null_semantics() {
    let ctx = TestContext::new("in_not_in");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE outer_vals (id UINT64 PRIMARY KEY, x INT64)")
        .unwrap();
    db.execute_statement("CREATE TABLE inner_vals (id UINT64 PRIMARY KEY, y INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO outer_vals VALUES (1, 10)")
        .unwrap();
    txn.execute_statement("INSERT INTO outer_vals VALUES (2, 20)")
        .unwrap();
    txn.execute_statement("INSERT INTO outer_vals VALUES (3, 30)")
        .unwrap();
    txn.execute_statement("INSERT INTO outer_vals VALUES (4, NULL)")
        .unwrap();

    txn.execute_statement("INSERT INTO inner_vals VALUES (1, 10)")
        .unwrap();
    txn.execute_statement("INSERT INTO inner_vals VALUES (2, NULL)")
        .unwrap();
    txn.execute_statement("INSERT INTO inner_vals VALUES (3, 40)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // Literal list IN / NOT IN
        "SELECT id, x FROM outer_vals WHERE x IN (10, 20)",
        "SELECT id, x FROM outer_vals WHERE x NOT IN (10, 20)",
        "SELECT id, x FROM outer_vals WHERE x IN (10, NULL)",
        "SELECT id, x FROM outer_vals WHERE x NOT IN (10, NULL)",
        "SELECT id, x FROM outer_vals WHERE x IN (NULL)",
        "SELECT id, x FROM outer_vals WHERE x NOT IN (NULL)",
        // Subquery IN / NOT IN
        "SELECT id, x FROM outer_vals WHERE x IN (SELECT y FROM inner_vals)",
        "SELECT id, x FROM outer_vals WHERE x NOT IN (SELECT y FROM inner_vals)",
        "SELECT id, x FROM outer_vals WHERE x IN (SELECT y FROM inner_vals WHERE y IS NOT NULL)",
        "SELECT id, x FROM outer_vals WHERE x NOT IN (SELECT y FROM inner_vals WHERE y IS NOT NULL)",
        "SELECT id, x FROM outer_vals WHERE x IN (SELECT y FROM inner_vals WHERE y > 1000)", // empty subquery
        "SELECT id, x FROM outer_vals WHERE x NOT IN (SELECT y FROM inner_vals WHERE y > 1000)", // empty subquery
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 5. Scalar Subqueries (Section 11)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_scalar_subqueries() {
    let ctx = TestContext::new("scalar_subquery");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE main (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();
    db.execute_statement("CREATE TABLE sub (id UINT64 PRIMARY KEY, score INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO main VALUES (1, 10)")
        .unwrap();
    txn.execute_statement("INSERT INTO main VALUES (2, 20)")
        .unwrap();
    txn.commit().unwrap();

    // 0 rows -> NULL
    let mut txn = db.begin_transaction().unwrap();
    let q_zero = "SELECT val, (SELECT score FROM sub WHERE id = 999) AS s FROM main";
    let opt_zero = txn.execute_sql(q_zero).unwrap();
    let ref_zero = txn.execute_sql_reference(q_zero).unwrap();
    assert_differential_equivalence(&opt_zero, &ref_zero, false);
    for row in &opt_zero.rows {
        assert_eq!(row.values[1], Value::Null);
    }
    txn.commit().unwrap();

    // 1 row -> scalar value
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO sub VALUES (1, 99)")
        .unwrap();
    let q_one = "SELECT val, (SELECT score FROM sub WHERE id = 1) AS s FROM main";
    let opt_one = txn.execute_sql(q_one).unwrap();
    let ref_one = txn.execute_sql_reference(q_one).unwrap();
    assert_differential_equivalence(&opt_one, &ref_one, false);
    for row in &opt_one.rows {
        assert_eq!(row.values[1], Value::Int64(99));
    }
    txn.commit().unwrap();

    // 2+ rows -> ScalarSubqueryMultipleRows error on both paths
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO sub VALUES (2, 100)")
        .unwrap();
    let q_multi = "SELECT val, (SELECT score FROM sub) AS s FROM main";
    let opt_err = txn.execute_sql(q_multi).unwrap_err();
    let ref_err = txn.execute_sql_reference(q_multi).unwrap_err();
    assert!(
        matches!(opt_err, Error::ScalarSubqueryMultipleRows),
        "Expected ScalarSubqueryMultipleRows in optimized execution, got {:?}",
        opt_err
    );
    assert!(
        matches!(ref_err, Error::ScalarSubqueryMultipleRows),
        "Expected ScalarSubqueryMultipleRows in reference execution, got {:?}",
        ref_err
    );
}

// -----------------------------------------------------------------------------
// 6. Correlated Subqueries (Section 12)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_correlated_subqueries() {
    let ctx = TestContext::new("correlated_subqueries");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE dept (id UINT64 PRIMARY KEY, name STRING, min_salary INT64)",
    )
    .unwrap();
    db.execute_statement("CREATE TABLE emp (id UINT64 PRIMARY KEY, dept_id UINT64, salary INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO dept VALUES (1, 'Engineering', 50000)")
        .unwrap();
    txn.execute_statement("INSERT INTO dept VALUES (2, 'HR', 40000)")
        .unwrap();
    txn.execute_statement("INSERT INTO dept VALUES (3, 'Legal', 60000)")
        .unwrap();

    txn.execute_statement("INSERT INTO emp VALUES (1, 1, 60000)")
        .unwrap();
    txn.execute_statement("INSERT INTO emp VALUES (2, 1, 45000)")
        .unwrap();
    txn.execute_statement("INSERT INTO emp VALUES (3, 2, 42000)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // Correlated EXISTS
        "SELECT id, name FROM dept WHERE EXISTS (SELECT 1 FROM emp WHERE emp.dept_id = dept.id)",
        // Correlated NOT EXISTS
        "SELECT id, name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE emp.dept_id = dept.id)",
        // Correlated scalar subquery in WHERE
        "SELECT id, name FROM dept WHERE min_salary < (SELECT MAX(salary) FROM emp WHERE emp.dept_id = dept.id)",
        // Correlated filter with salary threshold
        "SELECT id, name FROM dept WHERE EXISTS (SELECT 1 FROM emp WHERE emp.dept_id = dept.id AND emp.salary >= dept.min_salary)",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 7. JOIN Differential Stress Testing (Sections 13, 14, 15, 16)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_joins_all_types_and_safety() {
    let ctx = TestContext::new("joins_differential");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, k INT64, v1 STRING)")
        .unwrap();
    db.execute_statement("CREATE TABLE t2 (id UINT64 PRIMARY KEY, k INT64, v2 STRING)")
        .unwrap();
    db.execute_statement("CREATE TABLE t3 (id UINT64 PRIMARY KEY, k INT64, v3 STRING)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    // 1:N, N:1, N:M and NULL join keys
    txn.execute_statement("INSERT INTO t1 VALUES (1, 10, 'a1')")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 10, 'a2')")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (3, 20, 'a3')")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (4, NULL, 'a4')")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 10, 'b1')")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, 30, 'b2')")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (3, NULL, 'b3')")
        .unwrap();

    txn.execute_statement("INSERT INTO t3 VALUES (1, 10, 'c1')")
        .unwrap();
    txn.execute_statement("INSERT INTO t3 VALUES (2, 20, 'c2')")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // INNER JOIN
        "SELECT t1.id, t1.k, t2.v2 FROM t1 INNER JOIN t2 ON t1.k = t2.k",
        // LEFT JOIN
        "SELECT t1.id, t1.k, t2.v2 FROM t1 LEFT JOIN t2 ON t1.k = t2.k",
        // RIGHT JOIN
        "SELECT t1.id, t2.k, t2.v2 FROM t1 RIGHT JOIN t2 ON t1.k = t2.k",
        // FULL OUTER JOIN
        "SELECT t1.id, t1.k, t2.k, t2.v2 FROM t1 FULL JOIN t2 ON t1.k = t2.k",
        // Outer Join Safety (WHERE vs ON)
        "SELECT t1.id, t2.v2 FROM t1 LEFT JOIN t2 ON t1.k = t2.k WHERE t2.v2 IS NULL",
        "SELECT t1.id, t2.v2 FROM t1 LEFT JOIN t2 ON t1.k = t2.k WHERE t2.k = 10",
        "SELECT t1.id, t2.v2 FROM t1 LEFT JOIN t2 ON t1.k = t2.k AND t2.k = 10",
        // 3-way join with join reordering potential
        "SELECT t1.id, t1.k, t2.v2, t3.v3 FROM t1 INNER JOIN t2 ON t1.k = t2.k INNER JOIN t3 ON t1.k = t3.k",
        "SELECT t1.id, t2.v2, t3.v3 FROM t1 LEFT JOIN t2 ON t1.k = t2.k LEFT JOIN t3 ON t1.k = t3.k",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 8. GROUP BY and Aggregations (Sections 17, 18)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_group_by_and_aggregates() {
    let ctx = TestContext::new("group_by_agg");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE sales (id UINT64 PRIMARY KEY, region STRING, dept STRING, amount INT64, qty UINT64, discount FLOAT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (1, 'East', 'Toys', 100, 2, 0.1)")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (2, 'East', 'Toys', -20, 1, 0.05)")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (3, 'East', 'Books', 50, 5, 0.0)")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (4, 'West', 'Books', NULL, 3, 0.2)")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (5, 'West', NULL, 200, 10, NULL)")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (6, NULL, NULL, NULL, 0, NULL)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // Aggregation without GROUP BY (global scalars)
        "SELECT COUNT(*), COUNT(amount), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM sales",
        "SELECT SUM(qty), AVG(discount), MIN(region), MAX(region) FROM sales",
        // Empty table aggregation
        "SELECT COUNT(*), COUNT(amount), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM sales WHERE id > 999",
        // Single column GROUP BY (including NULL group)
        "SELECT region, COUNT(*), SUM(amount) FROM sales GROUP BY region",
        // Multiple column GROUP BY
        "SELECT region, dept, COUNT(*), SUM(amount), AVG(amount) FROM sales GROUP BY region, dept",
        // GROUP BY with HAVING
        "SELECT region, COUNT(*) FROM sales GROUP BY region HAVING COUNT(*) > 1",
        "SELECT region, SUM(amount) FROM sales GROUP BY region HAVING SUM(amount) > 50",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, false);
    }
}

// -----------------------------------------------------------------------------
// 9. Window Functions and Ordering Independence (Sections 19, 20)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_window_functions_and_ordering() {
    let ctx = TestContext::new("window_functions");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE emps (id UINT64 PRIMARY KEY, dept STRING, salary INT64, rank_key INT64)",
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (1, 'Sales', 5000, 10)")
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (2, 'Sales', 6000, 20)")
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (3, 'Sales', 5000, 10)") // tie in rank_key
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (4, 'Eng', 7000, 15)")
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (5, 'Eng', 8000, 30)")
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (6, NULL, 4000, 5)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        // ROW_NUMBER, RANK, DENSE_RANK
        "SELECT id, dept, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emps",
        "SELECT id, dept, salary, RANK() OVER (PARTITION BY dept ORDER BY salary ASC) AS rk, DENSE_RANK() OVER (PARTITION BY dept ORDER BY salary ASC) AS drk FROM emps",
        // LAG and LEAD
        "SELECT id, dept, salary, LAG(salary, 1, 0) OVER (PARTITION BY dept ORDER BY salary ASC) AS prev_s, LEAD(salary, 1, 0) OVER (PARTITION BY dept ORDER BY salary ASC) AS next_s FROM emps",
        // FIRST_VALUE and LAST_VALUE
        "SELECT id, dept, salary, FIRST_VALUE(salary) OVER (PARTITION BY dept ORDER BY salary ASC) AS fv, LAST_VALUE(salary) OVER (PARTITION BY dept ORDER BY salary ASC) AS lv FROM emps",
        // Window internal ORDER BY is independent of final query ORDER BY
        "SELECT id, dept, salary, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emps ORDER BY id DESC",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        let requires_order = sql.contains("ORDER BY id");
        assert_differential_equivalence(&opt, &ref_res, requires_order);
    }
}

// -----------------------------------------------------------------------------
// 10. Set Operations Differential Testing (Section 23)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_set_operations_multiset_semantics() {
    let ctx = TestContext::new("set_operations");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE s1 (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();
    db.execute_statement("CREATE TABLE s2 (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO s1 VALUES (1, 10)")
        .unwrap();
    txn.execute_statement("INSERT INTO s1 VALUES (2, 20)")
        .unwrap();
    txn.execute_statement("INSERT INTO s1 VALUES (3, 20)")
        .unwrap(); // duplicate in s1
    txn.execute_statement("INSERT INTO s1 VALUES (4, NULL)")
        .unwrap();

    txn.execute_statement("INSERT INTO s2 VALUES (1, 20)")
        .unwrap();
    txn.execute_statement("INSERT INTO s2 VALUES (2, 30)")
        .unwrap();
    txn.execute_statement("INSERT INTO s2 VALUES (3, NULL)")
        .unwrap();
    txn.commit().unwrap();

    let queries = [
        "SELECT val FROM s1 UNION SELECT val FROM s2",
        "SELECT val FROM s1 UNION ALL SELECT val FROM s2",
        "SELECT val FROM s1 INTERSECT SELECT val FROM s2",
        "SELECT val FROM s1 INTERSECT ALL SELECT val FROM s2",
        "SELECT val FROM s1 EXCEPT SELECT val FROM s2",
        "SELECT val FROM s1 EXCEPT ALL SELECT val FROM s2",
        // Set operations with final ORDER BY and LIMIT
        "SELECT val FROM s1 UNION ALL SELECT val FROM s2 ORDER BY val ASC LIMIT 4",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        let requires_order = sql.contains("ORDER BY");
        assert_differential_equivalence(&opt, &ref_res, requires_order);
    }
}

// -----------------------------------------------------------------------------
// 11. Access Path Differential Testing (Section 28)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_access_path_equivalence() {
    let ctx = TestContext::new("access_paths");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE items (id UINT64 PRIMARY KEY, category STRING, tag STRING, price INT64)",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_category ON items (category)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_cat_tag ON items (category, tag)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_covering ON items (category) INCLUDE (price)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=50 {
        let cat = match i % 3 {
            0 => "electronics",
            1 => "books",
            _ => "clothing",
        };
        let tag = if i % 2 == 0 { "sale" } else { "new" };
        let price = (i * 10) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO items VALUES ({i}, '{cat}', '{tag}', {price})"
        ))
        .unwrap();
    }
    txn.commit().unwrap();

    let queries = [
        // Primary key point lookup
        "SELECT id, category, price FROM items WHERE id = 15",
        // Primary key range scan
        "SELECT id, category, price FROM items WHERE id >= 10 AND id <= 25",
        // Secondary index lookup
        "SELECT id, category, price FROM items WHERE category = 'books'",
        // Composite index lookup
        "SELECT id, category, tag, price FROM items WHERE category = 'books' AND tag = 'sale'",
        // Covering index scan
        "SELECT category, price FROM items WHERE category = 'clothing'",
        // Ordered index scan with LIMIT
        "SELECT id, category, price FROM items WHERE id >= 5 ORDER BY id ASC LIMIT 5",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for sql in queries {
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        let requires_order = sql.contains("ORDER BY");
        assert_differential_equivalence(&opt, &ref_res, requires_order);
    }
}

// -----------------------------------------------------------------------------
// 12. Randomized Differential Stress Test (Section 43)
// -----------------------------------------------------------------------------

#[test]
fn test_differential_randomized_stress_suite() {
    let ctx = TestContext::new("randomized_stress");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE rand_data (id UINT64 PRIMARY KEY, val INT64, group_col STRING)",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_val ON rand_data (val)")
        .unwrap();

    // Deterministic pseudo-random generation using LCG
    let mut seed = 123456789u64;
    let mut next_rand = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    let mut txn = db.begin_transaction().unwrap();
    for id in 1..=100 {
        let r = next_rand();
        let val = if r % 7 == 0 {
            "NULL".to_string()
        } else {
            ((r % 50) as i64).to_string()
        };
        let group = match r % 4 {
            0 => "'alpha'",
            1 => "'beta'",
            2 => "'gamma'",
            _ => "NULL",
        };
        txn.execute_statement(&format!(
            "INSERT INTO rand_data VALUES ({id}, {val}, {group})"
        ))
        .unwrap();
    }
    txn.commit().unwrap();

    let queries = [
        "SELECT COUNT(*), SUM(val), AVG(val), MIN(val), MAX(val) FROM rand_data",
        "SELECT group_col, COUNT(*), SUM(val) FROM rand_data GROUP BY group_col",
        "SELECT id, val FROM rand_data WHERE val > 20 ORDER BY id ASC LIMIT 10",
        "SELECT id, val FROM rand_data WHERE val IS NULL OR val < 10",
        "SELECT r1.id, r1.val, r2.group_col FROM rand_data r1 INNER JOIN rand_data r2 ON r1.val = r2.val WHERE r1.id <= 20",
    ];

    let mut txn = db.begin_transaction().unwrap();
    for (step, sql) in queries.iter().enumerate() {
        let opt = txn
            .execute_sql(sql)
            .unwrap_or_else(|e| panic!("Step {step} failed on optimized execution: {e}"));
        let ref_res = txn
            .execute_sql_reference(sql)
            .unwrap_or_else(|e| panic!("Step {step} failed on reference execution: {e}"));
        let requires_order = sql.contains("ORDER BY");
        assert_differential_equivalence(&opt, &ref_res, requires_order);
    }
}
