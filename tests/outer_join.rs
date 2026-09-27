use std::path::PathBuf;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Result;
use treedb::query::external_hash_join::{analyze_equi_join, ExternalHashJoiner, JoinConfig};
use treedb::query::metrics::find_operator_metrics;
use treedb::query::OperatorKind;
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
            "test_outer_join_{}_{}_{}",
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
        let temp_dir = base_dir.join("temp_outer_runs");
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
            .count()
    }
}

fn assert_rows_sorted_equivalent(actual: &[Row], expected: &[Row]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "row count mismatch: actual={}, expected={}",
        actual.len(),
        expected.len()
    );

    let mut actual_sorted = actual.to_vec();
    let mut expected_sorted = expected.to_vec();

    let sort_fn = |a: &Row, b: &Row| {
        for (v1, v2) in a.values.iter().zip(b.values.iter()) {
            match (v1, v2) {
                (Value::Null, Value::Null) => continue,
                (Value::Null, _) => return std::cmp::Ordering::Less,
                (_, Value::Null) => return std::cmp::Ordering::Greater,
                (Value::Int64(x), Value::Int64(y)) => match x.cmp(y) {
                    std::cmp::Ordering::Equal => continue,
                    ord => return ord,
                },
                (Value::UInt64(x), Value::UInt64(y)) => match x.cmp(y) {
                    std::cmp::Ordering::Equal => continue,
                    ord => return ord,
                },
                (Value::String(x), Value::String(y)) => match x.cmp(y) {
                    std::cmp::Ordering::Equal => continue,
                    ord => return ord,
                },
                _ => {
                    let s1 = format!("{v1:?}");
                    let s2 = format!("{v2:?}");
                    match s1.cmp(&s2) {
                        std::cmp::Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
            }
        }
        std::cmp::Ordering::Equal
    };

    actual_sorted.sort_by(sort_fn);
    expected_sorted.sort_by(sort_fn);

    for (i, (act, exp)) in actual_sorted.iter().zip(expected_sorted.iter()).enumerate() {
        assert_eq!(
            act.values, exp.values,
            "mismatch at sorted index {i}: actual={:?}, expected={:?}",
            act.values, exp.values
        );
    }
}

// -----------------------------------------------------------------------------
// 1. Parser & Syntax: Case-insensitivity & Variants
// -----------------------------------------------------------------------------
#[test]
fn test_parser_outer_join_syntax_variants() -> Result<()> {
    let ctx = TestContext::new("syntax_variants");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v TEXT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v TEXT);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // RIGHT JOIN, right join, RIGHT OUTER JOIN, right outer join
    let _ = txn.execute_sql("SELECT * FROM t1 RIGHT JOIN t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 right join t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 RIGHT OUTER JOIN t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 right outer join t2 on t1.id = t2.id;")?;

    // FULL JOIN, full join, FULL OUTER JOIN, full outer join
    let _ = txn.execute_sql("SELECT * FROM t1 FULL JOIN t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 full join t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.id = t2.id;")?;
    let _ = txn.execute_sql("SELECT * FROM t1 full outer join t2 on t1.id = t2.id;")?;

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 2. RIGHT JOIN Semantics & Edge Cases
// -----------------------------------------------------------------------------
#[test]
fn test_right_join_semantics_and_edge_cases() -> Result<()> {
    let ctx = TestContext::new("right_join_semantics");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE emp (id BIGINT PRIMARY KEY, name TEXT, dept_id BIGINT);")?;
    txn.execute_statement("CREATE TABLE dept (id BIGINT PRIMARY KEY, dept_name TEXT);")?;

    txn.execute_statement(
        "INSERT INTO emp (id, name, dept_id) VALUES (1, 'Alice', 10), (2, 'Bob', 10), (3, 'Charlie', 20);",
    )?;
    txn.execute_statement(
        "INSERT INTO dept (id, dept_name) VALUES (10, 'Engineering'), (20, 'Sales'), (30, 'Marketing');",
    )?;

    // 1. Basic RIGHT JOIN: dept 30 has no employees -> emp cols become NULL
    let res = txn.execute_sql(
        "SELECT e.name, d.dept_name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id ORDER BY d.id, e.id;",
    )?;
    assert_eq!(res.rows.len(), 4);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Alice".to_string()),
            Value::String("Engineering".to_string())
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::String("Bob".to_string()),
            Value::String("Engineering".to_string())
        ]
    );
    assert_eq!(
        res.rows[2].values,
        vec![
            Value::String("Charlie".to_string()),
            Value::String("Sales".to_string())
        ]
    );
    assert_eq!(
        res.rows[3].values,
        vec![Value::Null, Value::String("Marketing".to_string())]
    );

    // 2. Empty left side
    txn.execute_statement("CREATE TABLE empty_l (id BIGINT PRIMARY KEY, dept_id BIGINT);")?;
    let res_empty_l = txn.execute_sql(
        "SELECT e.id, d.dept_name FROM empty_l e RIGHT JOIN dept d ON e.dept_id = d.id;",
    )?;
    assert_eq!(res_empty_l.rows.len(), 3);
    for row in res_empty_l.rows {
        assert_eq!(row.values[0], Value::Null);
        assert!(row.values[1] != Value::Null);
    }

    // 3. Empty right side
    txn.execute_statement("CREATE TABLE empty_r (id BIGINT PRIMARY KEY, name TEXT);")?;
    let res_empty_r = txn.execute_sql(
        "SELECT e.name, d.name FROM emp e RIGHT JOIN empty_r d ON e.dept_id = d.id;",
    )?;
    assert_eq!(res_empty_r.rows.len(), 0);

    // 4. All unmatched
    txn.execute_statement("CREATE TABLE other_dept (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement("INSERT INTO other_dept (id, name) VALUES (99, 'Legal'), (100, 'HR');")?;
    let res_all_unmatched = txn.execute_sql(
        "SELECT e.name, d.name FROM emp e RIGHT JOIN other_dept d ON e.dept_id = d.id ORDER BY d.id;",
    )?;
    assert_eq!(res_all_unmatched.rows.len(), 2);
    assert_eq!(
        res_all_unmatched.rows[0].values,
        vec![Value::Null, Value::String("Legal".to_string())]
    );
    assert_eq!(
        res_all_unmatched.rows[1].values,
        vec![Value::Null, Value::String("HR".to_string())]
    );

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 3. FULL JOIN Semantics & Edge Cases
// -----------------------------------------------------------------------------
#[test]
fn test_full_join_semantics_and_edge_cases() -> Result<()> {
    let ctx = TestContext::new("full_join_semantics");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k BIGINT, val TEXT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k BIGINT, note TEXT);")?;

    // a has keys 1, 2, 3 (3 has no match in b)
    txn.execute_statement(
        "INSERT INTO a (id, k, val) VALUES (1, 10, 'A1'), (2, 20, 'A2'), (3, 30, 'A3');",
    )?;
    // b has keys 1, 2, 4 (4 has no match in a)
    txn.execute_statement(
        "INSERT INTO b (id, k, note) VALUES (1, 10, 'B1'), (2, 20, 'B2'), (3, 40, 'B4');",
    )?;

    let res = txn.execute_sql("SELECT a.k, a.val, b.k, b.note FROM a FULL JOIN b ON a.k = b.k;")?;

    assert_eq!(res.rows.len(), 4);

    let expected = vec![
        Row::new(vec![
            Value::Int64(10),
            Value::String("A1".to_string()),
            Value::Int64(10),
            Value::String("B1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(20),
            Value::String("A2".to_string()),
            Value::Int64(20),
            Value::String("B2".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(30),
            Value::String("A3".to_string()),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Null,
            Value::Null,
            Value::Int64(40),
            Value::String("B4".to_string()),
        ]),
    ];
    assert_rows_sorted_equivalent(&res.rows, &expected);

    // Empty left side in FULL JOIN
    txn.execute_statement("CREATE TABLE empty_tbl (id BIGINT PRIMARY KEY, k BIGINT);")?;
    let full_empty_l = txn.execute_sql("SELECT * FROM empty_tbl e FULL JOIN b ON e.k = b.k;")?;
    assert_eq!(full_empty_l.rows.len(), 3);
    for r in full_empty_l.rows {
        assert_eq!(r.values[0], Value::Null);
        assert_eq!(r.values[1], Value::Null);
        assert!(r.values[2] != Value::Null);
    }

    // Empty right side in FULL JOIN
    let full_empty_r = txn.execute_sql("SELECT * FROM a FULL JOIN empty_tbl e ON a.k = e.k;")?;
    assert_eq!(full_empty_r.rows.len(), 3);
    for r in full_empty_r.rows {
        assert!(r.values[0] != Value::Null);
        assert_eq!(r.values[3], Value::Null);
        assert_eq!(r.values[4], Value::Null);
    }

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 4. NULL Keys & SQL 3-Valued Logic in Outer Joins
// -----------------------------------------------------------------------------
#[test]
fn test_null_keys_3vl_outer_join_semantics() -> Result<()> {
    let ctx = TestContext::new("null_keys_3vl");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_left (id BIGINT PRIMARY KEY, k BIGINT, tag TEXT);")?;
    txn.execute_statement("CREATE TABLE t_right (id BIGINT PRIMARY KEY, k BIGINT, info TEXT);")?;

    txn.execute_statement("INSERT INTO t_left (id, k, tag) VALUES (1, NULL, 'L_NULL_1'), (2, NULL, 'L_NULL_2'), (3, 10, 'L_10');")?;
    txn.execute_statement("INSERT INTO t_right (id, k, info) VALUES (1, NULL, 'R_NULL_1'), (2, 10, 'R_10'), (3, NULL, 'R_NULL_2');")?;

    // 1. RIGHT JOIN with NULL keys:
    let right_res =
        txn.execute_sql("SELECT l.tag, r.info FROM t_left l RIGHT JOIN t_right r ON l.k = r.k;")?;
    assert_eq!(right_res.rows.len(), 3);
    let expected_right = vec![
        Row::new(vec![
            Value::String("L_10".to_string()),
            Value::String("R_10".to_string()),
        ]),
        Row::new(vec![Value::Null, Value::String("R_NULL_1".to_string())]),
        Row::new(vec![Value::Null, Value::String("R_NULL_2".to_string())]),
    ];
    assert_rows_sorted_equivalent(&right_res.rows, &expected_right);

    // 2. FULL JOIN with NULL keys:
    let full_res =
        txn.execute_sql("SELECT l.tag, r.info FROM t_left l FULL JOIN t_right r ON l.k = r.k;")?;
    assert_eq!(full_res.rows.len(), 5);
    let expected_full = vec![
        Row::new(vec![
            Value::String("L_10".to_string()),
            Value::String("R_10".to_string()),
        ]),
        Row::new(vec![Value::String("L_NULL_1".to_string()), Value::Null]),
        Row::new(vec![Value::String("L_NULL_2".to_string()), Value::Null]),
        Row::new(vec![Value::Null, Value::String("R_NULL_1".to_string())]),
        Row::new(vec![Value::Null, Value::String("R_NULL_2".to_string())]),
    ];
    assert_rows_sorted_equivalent(&full_res.rows, &expected_full);

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 5. Composite Keys & Residual Predicates
// -----------------------------------------------------------------------------
#[test]
fn test_composite_keys_and_residual_predicates() -> Result<()> {
    let ctx = TestContext::new("composite_residual");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, cust_id BIGINT, region BIGINT, amount BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE targets (id BIGINT PRIMARY KEY, cust_id BIGINT, region BIGINT, min_target BIGINT);",
    )?;

    txn.execute_statement(
        "INSERT INTO orders (id, cust_id, region, amount) VALUES
            (1, 100, 1, 500),
            (2, 100, 2, 200),
            (3, 200, 1, 100);",
    )?;

    txn.execute_statement(
        "INSERT INTO targets (id, cust_id, region, min_target) VALUES
            (1, 100, 1, 300),
            (2, 100, 2, 400),
            (3, 300, 1, 100);",
    )?;

    // 1. Composite Equi-Join RIGHT JOIN
    let res_comp = txn.execute_sql(
        "SELECT o.id, t.id, t.min_target FROM orders o RIGHT JOIN targets t ON o.cust_id = t.cust_id AND o.region = t.region ORDER BY t.id;",
    )?;
    assert_eq!(res_comp.rows.len(), 3);
    assert_eq!(
        res_comp.rows[0].values,
        vec![Value::Int64(1), Value::Int64(1), Value::Int64(300)]
    );
    assert_eq!(
        res_comp.rows[1].values,
        vec![Value::Int64(2), Value::Int64(2), Value::Int64(400)]
    );
    assert_eq!(
        res_comp.rows[2].values,
        vec![Value::Null, Value::Int64(3), Value::Int64(100)]
    );

    // 2. Residual Predicate with FULL JOIN:
    let res_res = txn.execute_sql(
        "SELECT o.id, o.amount, t.id, t.min_target FROM orders o FULL JOIN targets t ON o.cust_id = t.cust_id AND o.region = t.region AND o.amount >= t.min_target;",
    )?;
    assert_eq!(res_res.rows.len(), 5);

    let exp_res = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Int64(500),
            Value::Int64(1),
            Value::Int64(300),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Int64(200),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Null,
            Value::Null,
            Value::Int64(2),
            Value::Int64(400),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Int64(100),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Null,
            Value::Null,
            Value::Int64(3),
            Value::Int64(100),
        ]),
    ];
    assert_rows_sorted_equivalent(&res_res.rows, &exp_res);

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 6. External Spill-to-Disk Execution for RIGHT & FULL JOIN
// -----------------------------------------------------------------------------
#[test]
fn test_external_spill_outer_join_execution() -> Result<()> {
    let ctx = TestContext::new("spill_outer");
    let mut db = ctx.db();
    db.set_join_config(
        JoinConfig::new()
            .with_memory_budget_bytes(256)
            .with_max_temp_files(4)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE left_rel (id BIGINT PRIMARY KEY, k BIGINT, v TEXT);")?;
    txn.execute_statement("CREATE TABLE right_rel (id BIGINT PRIMARY KEY, k BIGINT, note TEXT);")?;

    for i in 1..=100 {
        let k = (i % 20) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO left_rel (id, k, v) VALUES ({i}, {k}, 'L_{i}');"
        ))?;
    }
    for j in 1..=80 {
        let k = ((j % 20) + 10) as i64; // keys 10..29
        txn.execute_statement(&format!(
            "INSERT INTO right_rel (id, k, note) VALUES ({j}, {k}, 'R_{j}');"
        ))?;
    }

    // Commit data before querying
    txn.commit()?;

    // RIGHT JOIN spilled execution
    let mut txn_spill = db.begin_transaction()?;
    let (right_res, metrics_r) = txn_spill.execute_sql_with_metrics(
        "SELECT l.k, l.v, r.k, r.note FROM left_rel l RIGHT JOIN right_rel r ON l.k = r.k;",
    )?;
    assert!(metrics_r.total_rows > 0);
    let root_r = metrics_r.root_operator.as_ref().unwrap();
    let join_op_r = find_operator_metrics(root_r, &|k| {
        matches!(
            k,
            OperatorKind::ExternalHashJoin { .. } | OperatorKind::HashJoin { .. }
        )
    })
    .unwrap();
    assert!(
        join_op_r.counters.bytes_spilled > 0,
        "should have spilled partitions to disk"
    );

    // FULL JOIN spilled execution
    let (full_res, metrics_f) = txn_spill.execute_sql_with_metrics(
        "SELECT l.k, l.v, r.k, r.note FROM left_rel l FULL JOIN right_rel r ON l.k = r.k;",
    )?;
    assert!(metrics_f.total_rows >= metrics_r.total_rows);
    let root_f = metrics_f.root_operator.as_ref().unwrap();
    let join_op_f = find_operator_metrics(root_f, &|k| {
        matches!(
            k,
            OperatorKind::ExternalHashJoin { .. } | OperatorKind::HashJoin { .. }
        )
    })
    .unwrap();
    assert!(
        join_op_f.counters.bytes_spilled > 0,
        "should have spilled partitions to disk"
    );
    txn_spill.commit()?;

    // Verify against in-memory large budget execution
    let mut in_mem_db = ctx.db();
    in_mem_db.set_join_config(JoinConfig::new().with_memory_budget_bytes(10 * 1024 * 1024));
    let mut in_mem_txn = in_mem_db.begin_transaction()?;

    let in_mem_right = in_mem_txn.execute_sql(
        "SELECT l.k, l.v, r.k, r.note FROM left_rel l RIGHT JOIN right_rel r ON l.k = r.k;",
    )?;
    let in_mem_full = in_mem_txn.execute_sql(
        "SELECT l.k, l.v, r.k, r.note FROM left_rel l FULL JOIN right_rel r ON l.k = r.k;",
    )?;

    assert_rows_sorted_equivalent(&right_res.rows, &in_mem_right.rows);
    assert_rows_sorted_equivalent(&full_res.rows, &in_mem_full.rows);

    in_mem_txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 7. Non-Equi Joins (Nested-Loop Fallback) for RIGHT & FULL JOIN
// -----------------------------------------------------------------------------
#[test]
fn test_non_equi_outer_join_fallback() -> Result<()> {
    let ctx = TestContext::new("non_equi");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE r1 (id BIGINT PRIMARY KEY, val BIGINT);")?;
    txn.execute_statement("CREATE TABLE r2 (id BIGINT PRIMARY KEY, thresh BIGINT);")?;

    txn.execute_statement("INSERT INTO r1 (id, val) VALUES (1, 10), (2, 25);")?;
    txn.execute_statement("INSERT INTO r2 (id, thresh) VALUES (1, 20), (2, 50);")?;

    // Non-equi RIGHT JOIN: `ON r1.val < r2.thresh`
    let res_right =
        txn.execute_sql("SELECT r1.val, r2.thresh FROM r1 RIGHT JOIN r2 ON r1.val < r2.thresh;")?;
    assert_eq!(res_right.rows.len(), 3);

    // Non-equi FULL JOIN where some right rows have no matches:
    txn.execute_statement("INSERT INTO r2 (id, thresh) VALUES (3, 5);")?;
    let res_full =
        txn.execute_sql("SELECT r1.val, r2.thresh FROM r1 FULL JOIN r2 ON r1.val < r2.thresh;")?;
    assert_eq!(res_full.rows.len(), 4);

    let exp_full = vec![
        Row::new(vec![Value::Int64(10), Value::Int64(20)]),
        Row::new(vec![Value::Int64(10), Value::Int64(50)]),
        Row::new(vec![Value::Int64(25), Value::Int64(50)]),
        Row::new(vec![Value::Null, Value::Int64(5)]),
    ];
    assert_rows_sorted_equivalent(&res_full.rows, &exp_full);

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 8. Multi-Join Composition & Associations
// -----------------------------------------------------------------------------
#[test]
fn test_multi_join_composition() -> Result<()> {
    let ctx = TestContext::new("multi_join");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_a (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement("CREATE TABLE t_b (id BIGINT PRIMARY KEY, a_id BIGINT, role TEXT);")?;
    txn.execute_statement("CREATE TABLE t_c (id BIGINT PRIMARY KEY, b_id BIGINT, loc TEXT);")?;

    txn.execute_statement("INSERT INTO t_a (id, name) VALUES (1, 'Alice'), (2, 'Bob');")?;
    txn.execute_statement(
        "INSERT INTO t_b (id, a_id, role) VALUES (10, 1, 'Dev'), (20, 3, 'Manager');",
    )?;
    txn.execute_statement(
        "INSERT INTO t_c (id, b_id, loc) VALUES (100, 10, 'Remote'), (200, 30, 'HQ');",
    )?;

    // Chain: t_a LEFT JOIN t_b ON t_a.id = t_b.a_id RIGHT JOIN t_c ON t_b.id = t_c.b_id
    let res = txn.execute_sql(
        "SELECT a.name, b.role, c.loc FROM t_a a LEFT JOIN t_b b ON a.id = b.a_id RIGHT JOIN t_c c ON b.id = c.b_id;",
    )?;
    assert_eq!(res.rows.len(), 2);
    let expected = vec![
        Row::new(vec![
            Value::String("Alice".to_string()),
            Value::String("Dev".to_string()),
            Value::String("Remote".to_string()),
        ]),
        Row::new(vec![
            Value::Null,
            Value::Null,
            Value::String("HQ".to_string()),
        ]),
    ];
    assert_rows_sorted_equivalent(&res.rows, &expected);

    // Chain: t_a FULL JOIN t_b ON a.id = b.a_id JOIN t_c ON b.id = c.b_id
    let res2 = txn.execute_sql(
        "SELECT a.name, b.role, c.loc FROM t_a a FULL JOIN t_b b ON a.id = b.a_id JOIN t_c c ON b.id = c.b_id;",
    )?;
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(
        res2.rows[0].values,
        vec![
            Value::String("Alice".to_string()),
            Value::String("Dev".to_string()),
            Value::String("Remote".to_string())
        ]
    );

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 9. SQL Subsystems Integration: WHERE, GROUP BY, Windows, CTEs, Set Ops
// -----------------------------------------------------------------------------
#[test]
fn test_outer_join_sql_subsystems_integration() -> Result<()> {
    let ctx = TestContext::new("sql_subsystems");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE depts (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE emps (id BIGINT PRIMARY KEY, dept_id BIGINT, salary BIGINT);",
    )?;

    txn.execute_statement(
        "INSERT INTO depts (id, name) VALUES (1, 'IT'), (2, 'HR'), (3, 'Sales');",
    )?;
    txn.execute_statement("INSERT INTO emps (id, dept_id, salary) VALUES (10, 1, 6000), (20, 1, 8000), (30, 2, 5000);")?;

    // 1. RIGHT JOIN + GROUP BY + HAVING
    let grp_res = txn.execute_sql(
        "SELECT d.name, COUNT(e.id), AVG(e.salary) FROM emps e RIGHT JOIN depts d ON e.dept_id = d.id GROUP BY d.name ORDER BY d.name;",
    )?;
    assert_eq!(grp_res.rows.len(), 3);
    assert_eq!(
        grp_res.rows[0].values,
        vec![
            Value::String("HR".to_string()),
            Value::UInt64(1),
            Value::Float64(5000.0)
        ]
    );
    assert_eq!(
        grp_res.rows[1].values,
        vec![
            Value::String("IT".to_string()),
            Value::UInt64(2),
            Value::Float64(7000.0)
        ]
    );
    assert_eq!(
        grp_res.rows[2].values,
        vec![
            Value::String("Sales".to_string()),
            Value::UInt64(0),
            Value::Null
        ]
    );

    // 2. FULL JOIN + Window Function
    let win_res = txn.execute_sql(
        "SELECT d.name, e.salary, ROW_NUMBER() OVER (PARTITION BY d.id ORDER BY e.salary) FROM emps e FULL JOIN depts d ON e.dept_id = d.id;",
    )?;
    assert_eq!(win_res.rows.len(), 4);

    // 3. RIGHT JOIN + CTE
    let cte_res = txn.execute_sql(
        "WITH high_sal AS (SELECT * FROM emps WHERE salary > 5500)
         SELECT d.name, h.salary FROM high_sal h RIGHT JOIN depts d ON h.dept_id = d.id ORDER BY d.name, h.salary;",
    )?;
    assert_eq!(cte_res.rows.len(), 4);

    // 4. FULL JOIN + Set Operations (UNION ALL)
    let set_res = txn.execute_sql(
        "SELECT d.name FROM emps e FULL JOIN depts d ON e.dept_id = d.id
         UNION ALL
         SELECT name FROM depts;",
    )?;
    assert_eq!(set_res.rows.len(), 7);

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 10. Prepared Statements & Plan Cache
// -----------------------------------------------------------------------------
#[test]
fn test_prepared_statements_and_plan_cache_outer_join() -> Result<()> {
    let ctx = TestContext::new("prep_cache");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE cat (id BIGINT PRIMARY KEY, name TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE prod (id BIGINT PRIMARY KEY, cat_id BIGINT, price BIGINT);",
    )?;

    txn.execute_statement("INSERT INTO cat (id, name) VALUES (1, 'Books'), (2, 'Games');")?;
    txn.execute_statement("INSERT INTO prod (id, cat_id, price) VALUES (10, 1, 20), (20, 1, 50);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let prep_stmt = txn.prepare_sql(
        "SELECT c.name, p.price FROM prod p RIGHT JOIN cat c ON p.cat_id = c.id WHERE c.id = ?;",
    )?;
    let (res1, metrics1) = txn.execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(1)])?;
    assert_eq!(res1.rows.len(), 2);
    assert!(!metrics1.plan_cache_hit);

    // Repeated execution with identical parameter: cache hit
    let (res1_repeat, metrics1_repeat) =
        txn.execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(1)])?;
    assert_eq!(res1_repeat.rows.len(), 2);
    assert!(metrics1_repeat.plan_cache_hit);

    let (res2, _) = txn.execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(2)])?;
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(
        res2.rows[0].values,
        vec![Value::String("Games".to_string()), Value::Null]
    );

    // Unparameterized prepared join query plan cache reuse
    let unparam_stmt =
        txn.prepare_sql("SELECT c.name, p.price FROM prod p RIGHT JOIN cat c ON p.cat_id = c.id;")?;
    let (res3, metrics3) = txn.execute_prepared_with_metrics(&unparam_stmt, &[])?;
    assert_eq!(res3.rows.len(), 3);
    assert!(!metrics3.plan_cache_hit);

    let (res4, metrics4) = txn.execute_prepared_with_metrics(&unparam_stmt, &[])?;
    assert_eq!(res4.rows.len(), 3);
    assert!(metrics4.plan_cache_hit);

    txn.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// 11. Transaction Rollback & Persistence
// -----------------------------------------------------------------------------
#[test]
fn test_transaction_rollback_and_persistence_outer_join() -> Result<()> {
    let ctx = TestContext::new("rollback_persistence");
    let mut db = ctx.db();
    db.set_join_config(
        JoinConfig::new()
            .with_memory_budget_bytes(128)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t_a (id BIGINT PRIMARY KEY, val BIGINT);")?;
    txn.execute_statement("CREATE TABLE t_b (id BIGINT PRIMARY KEY, val BIGINT);")?;

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t_a (id, val) VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!(
            "INSERT INTO t_b (id, val) VALUES ({i}, {i} + 10);"
        ))?;
    }
    txn.commit()?;

    // Read-only transaction executing spilled FULL JOIN then aborting
    let mut ro_txn = db.begin_transaction()?;
    let res = ro_txn.execute_sql("SELECT COUNT(*) FROM t_a a FULL JOIN t_b b ON a.val = b.val;")?;
    assert_eq!(res.rows[0].values[0], Value::UInt64(60)); // 40 matched + 10 unmatched left + 10 unmatched right = 60
    ro_txn.abort()?;

    assert_eq!(
        ctx.count_temp_files(),
        0,
        "all temporary files cleaned up on abort"
    );

    // Close and reopen database
    drop(db);
    let mut reopened_db = ctx.db();
    let mut check_txn = reopened_db.begin_transaction()?;
    let res2 =
        check_txn.execute_sql("SELECT COUNT(*) FROM t_a a FULL JOIN t_b b ON a.val = b.val;")?;
    assert_eq!(res2.rows[0].values[0], Value::UInt64(60));
    check_txn.commit()?;

    Ok(())
}

// -----------------------------------------------------------------------------
// 12. Randomized Differential Testing Oracle (Nested-Loop vs In-Memory vs Spilled)
// -----------------------------------------------------------------------------
#[test]
fn test_randomized_differential_outer_join_oracle() -> Result<()> {
    let ctx = TestContext::new("rand_oracle");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE rand_left (id BIGINT PRIMARY KEY, k BIGINT, v TEXT);")?;
    txn.execute_statement(
        "CREATE TABLE rand_right (id BIGINT PRIMARY KEY, k BIGINT, payload TEXT);",
    )?;

    // Seed data with duplicates, non-matches, and NULLs
    txn.execute_statement(
        "INSERT INTO rand_left (id, k, v) VALUES
            (1, 10, 'L1'), (2, 10, 'L2'), (3, 20, 'L3'), (4, 30, 'L4'), (5, NULL, 'L5'), (6, 50, 'L6');",
    )?;
    txn.execute_statement(
        "INSERT INTO rand_right (id, k, payload) VALUES
            (1, 10, 'R1'), (2, 20, 'R2'), (3, 20, 'R3'), (4, 40, 'R4'), (5, NULL, 'R5'), (6, 60, 'R6');",
    )?;

    let left_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("k", DataType::Int64, true),
        Column::new("v", DataType::String, true),
    ]);
    let right_schema = Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("k", DataType::Int64, true),
        Column::new("payload", DataType::String, true),
    ]);

    let all_left = txn.execute_sql("SELECT id, k, v FROM rand_left;")?.rows;
    let all_right = txn
        .execute_sql("SELECT id, k, payload FROM rand_right;")?
        .rows;

    let on_expr = treedb::query::expr::Expr::Binary {
        left: Box::new(treedb::query::expr::Expr::Column("k".to_string())),
        op: treedb::query::expr::BinaryOp::Eq,
        right: Box::new(treedb::query::expr::Expr::Column("k".to_string())),
    };

    for join_type in [JoinType::Right, JoinType::Full] {
        // 1. Nested-Loop reference
        let (_, nl_rows, _) = treedb::query::operator::JoinOperator::apply_with_config(
            all_left.clone(),
            &left_schema,
            &all_right,
            &right_schema,
            join_type,
            &on_expr,
            &JoinConfig::default(),
        )?;

        // 2. In-Memory Hash Join
        let equi_plan = analyze_equi_join(&on_expr, &left_schema, &right_schema).unwrap();
        let in_mem_config = JoinConfig::new().with_memory_budget_bytes(10 * 1024 * 1024);
        let in_mem_joiner = ExternalHashJoiner::new(
            &left_schema,
            &right_schema,
            join_type,
            equi_plan.clone(),
            in_mem_config,
        );
        let (_, in_mem_rows, _) = in_mem_joiner.execute(all_left.clone(), all_right.clone())?;

        // 3. Spilled External Hash Join
        let spill_config = JoinConfig::new()
            .with_memory_budget_bytes(64)
            .with_temp_dir(&ctx.temp_dir);
        let spill_joiner = ExternalHashJoiner::new(
            &left_schema,
            &right_schema,
            join_type,
            equi_plan,
            spill_config,
        );
        let (_, spill_rows, metrics) = spill_joiner.execute(all_left.clone(), all_right.clone())?;
        assert!(
            metrics.bytes_spilled > 0,
            "should have spilled for {join_type:?}"
        );

        // Compare all three implementations
        assert_rows_sorted_equivalent(&nl_rows, &in_mem_rows);
        assert_rows_sorted_equivalent(&in_mem_rows, &spill_rows);
    }

    txn.commit()?;
    Ok(())
}
