use tempfile::TempDir;
use treedb::engine::types::{Row, Value};
use treedb::query::external_hash_join::JoinConfig;
use treedb::query::runtime_filter::RuntimeFilterType;
use treedb::TreeDb;

fn create_test_db() -> (TreeDb, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db = TreeDb::create(temp_dir.path().join("test.db"), 64).unwrap();
    (db, temp_dir)
}

fn query(txn: &mut treedb::engine::Transaction, sql: &str) -> Vec<Row> {
    txn.execute_sql(sql).unwrap().rows
}

fn find_join_metrics(
    metrics: &treedb::query::QueryExecutionMetrics,
) -> &treedb::query::OperatorMetrics {
    metrics
        .root_operator
        .as_ref()
        .unwrap()
        .find_first(&|m| {
            matches!(
                m.kind,
                treedb::query::OperatorKind::HashJoin { .. }
                    | treedb::query::OperatorKind::ExternalHashJoin { .. }
                    | treedb::query::OperatorKind::NestedLoopJoin { .. }
                    | treedb::query::OperatorKind::HashSemiJoin
                    | treedb::query::OperatorKind::HashAntiJoin
                    | treedb::query::OperatorKind::NestedLoopSemiJoin
                    | treedb::query::OperatorKind::NestedLoopAntiJoin
            )
        })
        .expect("Join operator not found in metrics")
}

// ----------------------------------------------------------------------------
// Basic Tests (R411 - R417)
// ----------------------------------------------------------------------------

#[test]
fn test_r411_basic_inner_join_runtime_filter() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, amount INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE customers (id INT PRIMARY KEY, name STRING);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {i}, {i} * 10);"))
            .unwrap();
    }
    // Only 4 customers
    for &cid in &[10, 20, 42, 71] {
        txn.execute_statement(&format!(
            "INSERT INTO customers VALUES ({cid}, 'Cust_{cid}');"
        ))
        .unwrap();
    }

    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT o.id, c.name FROM orders o JOIN customers c ON o.customer_id = c.id;",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 4);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.runtime_filter_enabled);
    assert_eq!(jm.counters.runtime_filter_build_rows, 4);
    assert_eq!(jm.counters.runtime_filter_distinct_keys, 4);
    assert_eq!(jm.counters.runtime_filter_probe_rows, 100);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 4);
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 96);
}

#[test]
fn test_r412_filter_disabled_produces_identical_result() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE customers (id INT PRIMARY KEY, name STRING);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {i});"))
            .unwrap();
    }
    for &cid in &[5, 15, 25, 35] {
        txn.execute_statement(&format!(
            "INSERT INTO customers VALUES ({cid}, 'Cust_{cid}');"
        ))
        .unwrap();
    }

    // Execution with filter enabled
    let res_enabled = query(
        &mut txn,
        "SELECT o.id, c.name FROM orders o JOIN customers c ON o.customer_id = c.id ORDER BY o.id;",
    );

    // Disable filter via join config
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_enable_runtime_filter(false));

    let (res_disabled, metrics_disabled) = txn
        .execute_sql_with_metrics(
            "SELECT o.id, c.name FROM orders o JOIN customers c ON o.customer_id = c.id ORDER BY o.id;",
        )
        .unwrap();

    assert_eq!(res_enabled, res_disabled.rows);
    let jm = find_join_metrics(&metrics_disabled);
    assert!(!jm.counters.runtime_filter_enabled);
}

#[test]
fn test_r413_exact_membership() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i} * 2);"))
            .unwrap();
    }
    for i in 11..=15 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i} * 2);"))
            .unwrap();
    }

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT a.id, b.id FROM a JOIN b ON a.id = b.id;")
        .unwrap();

    assert_eq!(res.rows.len(), 5);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.runtime_filter_enabled);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 5);
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 15);
    assert_eq!(jm.counters.runtime_filter_false_positive_rows, 0);
}

#[test]
fn test_r414_duplicate_build_keys() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE probe_tbl (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE build_tbl (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO probe_tbl VALUES ({i}, {i});"))
            .unwrap();
    }
    // Duplicate keys on build side
    txn.execute_statement("INSERT INTO build_tbl VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO build_tbl VALUES (2, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO build_tbl VALUES (3, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO build_tbl VALUES (4, 20);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT p.id, b.id FROM probe_tbl p JOIN build_tbl b ON p.k = b.k;",
        )
        .unwrap();

    // Key 10 matches 2 rows, Key 20 matches 2 rows -> total 4 output rows
    assert_eq!(res.rows.len(), 4);
    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_build_rows, 4);
    assert_eq!(jm.counters.runtime_filter_distinct_keys, 2);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 2);
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 28);
}

#[test]
fn test_r415_duplicate_probe_keys() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE probe_tbl (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE build_tbl (id INT PRIMARY KEY, k INT);")
        .unwrap();

    // Duplicate keys on probe side
    txn.execute_statement("INSERT INTO probe_tbl VALUES (1, 5);")
        .unwrap();
    txn.execute_statement("INSERT INTO probe_tbl VALUES (2, 5);")
        .unwrap();
    txn.execute_statement("INSERT INTO probe_tbl VALUES (3, 5);")
        .unwrap();
    txn.execute_statement("INSERT INTO probe_tbl VALUES (4, 99);")
        .unwrap();

    txn.execute_statement("INSERT INTO build_tbl VALUES (1, 5);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT p.id, b.id FROM probe_tbl p JOIN build_tbl b ON p.k = b.k;",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 3);
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 1);
}

#[test]
fn test_r416_empty_build() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i});"))
            .unwrap();
    }

    let res = query(&mut txn, "SELECT * FROM p JOIN b ON p.id = b.id;");
    assert!(res.is_empty());
}

#[test]
fn test_r417_empty_probe() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i});"))
            .unwrap();
    }

    let res = query(&mut txn, "SELECT * FROM p JOIN b ON p.id = b.id;");
    assert!(res.is_empty());
}

// ----------------------------------------------------------------------------
// NULL / 3VL Tests (R418 - R422)
// ----------------------------------------------------------------------------

#[test]
fn test_r418_null_build_keys() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO p VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO p VALUES (2, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO p VALUES (3, 20);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (2, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (3, NULL);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT p.id, b.id FROM p JOIN b ON p.k = b.k;")
        .unwrap();

    // NULL does not match NULL
    assert_eq!(res.rows.len(), 1);
    let jm = find_join_metrics(&metrics);
    // Build side had 1 valid key and 2 NULLs -> filter contains only 1 key
    assert_eq!(jm.counters.runtime_filter_build_rows, 1);
    assert_eq!(jm.counters.runtime_filter_distinct_keys, 1);
    assert_eq!(jm.counters.runtime_filter_null_probe_rows, 1);
}

#[test]
fn test_r419_null_probe_keys() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, NULL);"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO b VALUES (1, 10);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT p.id, b.id FROM p JOIN b ON p.k = b.k;")
        .unwrap();

    assert!(res.is_empty());
    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_null_probe_rows, 10);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 0);
}

#[test]
fn test_r420_null_heavy_data() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=50 {
        let k_val = if i % 2 == 0 {
            format!("{i}")
        } else {
            "NULL".to_string()
        };
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, {k_val});"))
            .unwrap();
    }
    for i in 1..=20 {
        let k_val = if i % 4 == 0 {
            format!("{i}")
        } else {
            "NULL".to_string()
        };
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {k_val});"))
            .unwrap();
    }

    let res_filter_on = query(&mut txn, "SELECT p.id, b.id FROM p JOIN b ON p.k = b.k;");

    txn.db_mut()
        .set_join_config(JoinConfig::new().with_enable_runtime_filter(false));
    let res_filter_off = query(&mut txn, "SELECT p.id, b.id FROM p JOIN b ON p.k = b.k;");

    assert_eq!(res_filter_on, res_filter_off);
}

#[test]
fn test_r421_residual_predicate_with_null() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT, v INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO p VALUES (1, 10, 50);")
        .unwrap();
    txn.execute_statement("INSERT INTO p VALUES (2, 10, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (1, 10, 20);")
        .unwrap();

    // Residual predicate: p.v > b.v
    let res = query(
        &mut txn,
        "SELECT p.id, b.id FROM p JOIN b ON p.k = b.k AND p.v > b.v;",
    );
    // NULL > 20 is UNKNOWN (false), only row 1 matches
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
}

#[test]
fn test_r422_3vl_equivalence() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, c INT, d INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, NULL, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (3, 2, NULL);")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, NULL, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (3, 2, 30);")
        .unwrap();

    let res1 = query(&mut txn, "SELECT * FROM t1 JOIN t2 ON t1.a = t2.c;");
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_enable_runtime_filter(false));
    let res2 = query(&mut txn, "SELECT * FROM t1 JOIN t2 ON t1.a = t2.c;");

    assert_eq!(res1, res2);
}

// ----------------------------------------------------------------------------
// Composite Keys (R423 - R426)
// ----------------------------------------------------------------------------

#[test]
fn test_r423_composite_join_key() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, tenant INT, cust INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE customers (id INT PRIMARY KEY, tenant INT, cust INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, {tenant}, {cust});",
            tenant = i % 3,
            cust = i
        ))
        .unwrap();
    }
    for &cust in &[3, 6, 9] {
        let tenant = cust % 3;
        txn.execute_statement(&format!(
            "INSERT INTO customers VALUES ({cust}, {tenant}, {cust});"
        ))
        .unwrap();
    }

    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT o.id, c.id FROM orders o JOIN customers c ON o.tenant = c.tenant AND o.cust = c.cust;",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.runtime_filter_enabled);
    assert_eq!(jm.counters.runtime_filter_distinct_keys, 3);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 3);
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 17);
}

#[test]
fn test_r424_duplicate_composite_keys() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k1 INT, k2 INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k1 INT, k2 INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 10, 20);")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 10, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, 10, 20);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.k1 = t2.k1 AND t1.k2 = t2.k2;",
    );
    assert_eq!(res.len(), 4);
}

#[test]
fn test_r425_null_component_composite() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k1 INT, k2 INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k1 INT, k2 INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 10, 20);")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 10, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, 10, 20);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.k1 = t2.k1 AND t1.k2 = t2.k2;",
        )
        .unwrap();

    // Partial NULL key does not match
    assert_eq!(res.rows.len(), 1);
    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_null_probe_rows, 1);
}

#[test]
fn test_r426_composite_residual_predicate() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k1 INT, k2 INT, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k1 INT, k2 INT, val INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10, 20, 100);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 10, 20, 50);")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 10, 20, 80);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.k1 = t2.k1 AND t1.k2 = t2.k2 AND t1.val > t2.val;",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
}

// ----------------------------------------------------------------------------
// Cost Model & Decision (R427 - R431)
// ----------------------------------------------------------------------------

#[test]
fn test_r427_filter_selected_when_beneficial() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE big (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE small (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO big VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO small VALUES ({i}, {i});"))
            .unwrap();
    }

    let plan = txn
        .explain_sql("SELECT * FROM big b JOIN small s ON b.k = s.k;")
        .unwrap();
    assert!(plan.contains("Runtime Filter"));
}

#[test]
fn test_r428_filter_rejected_when_overhead_dominates() {
    let config = JoinConfig::new().with_enable_runtime_filter(false);
    let plan = treedb::query::runtime_filter::should_build_runtime_filter(
        treedb::sql::ast::JoinType::Inner,
        &[treedb::query::expr::Expr::Column("k".into())],
        10.0,
        10.0,
        Some(10.0),
        64 * 1024,
        &config,
    );
    assert!(plan.is_none());
}

#[test]
fn test_r429_statistics_sensitive_decision() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=100 {
        let k = i % 5;
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {k});"))
            .unwrap();
    }

    txn.execute_statement("ANALYZE p;").unwrap();
    txn.execute_statement("ANALYZE b;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM p JOIN b ON p.k = b.k;")
        .unwrap();
    assert!(plan.contains("Runtime Filter"));
}

#[test]
fn test_r430_missing_statistics_fallback() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=4 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
    }

    // No ANALYZE -> uses conservative heuristic and succeeds
    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM p JOIN b ON p.k = b.k;")
        .unwrap();
    assert_eq!(res.rows.len(), 4);
    assert!(find_join_metrics(&metrics).counters.runtime_filter_enabled);
}

#[test]
fn test_r431_resource_budget_sensitive_decision() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
    }

    // Set a tiny memory budget of 10 bytes -> exact filter disabled or bounded
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(10));

    let (res, _metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM p JOIN b ON p.k = b.k;")
        .unwrap();
    assert_eq!(res.rows.len(), 10);
}

// ----------------------------------------------------------------------------
// Hash Join (R432 - R435)
// ----------------------------------------------------------------------------

#[test]
fn test_r432_in_memory_hash_join() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.val = t2.val;")
        .unwrap();

    assert_eq!(res.rows.len(), 10);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.runtime_filter_enabled);
    assert_eq!(jm.counters.hash_build_rows, 10);
}

#[test]
fn test_r433_runtime_filter_during_hash_build() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let (_res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.val = t2.val;")
        .unwrap();

    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_build_rows, 5);
    assert_eq!(jm.counters.runtime_filter_distinct_keys, 5);
    assert!(jm.counters.runtime_filter_memory_bytes > 0);
}

#[test]
fn test_r434_runtime_filter_before_probe_lookup() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO t2 VALUES (1, 42);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();

    assert_eq!(res.rows.len(), 1);
    let jm = find_join_metrics(&metrics);
    // 99 probe rows rejected before hash lookup
    assert_eq!(jm.counters.runtime_filter_rejected_rows, 99);
    assert_eq!(jm.counters.runtime_filter_passed_rows, 1);
    assert_eq!(jm.counters.hash_probe_rows, 1);
}

#[test]
fn test_r435_hash_lookup_misses_distinguished_from_filter_rejects() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    // Configure Bloom filter which could theoretically produce false positives
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_runtime_filter_type(RuntimeFilterType::Bloom));

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();

    assert_eq!(res.rows.len(), 5);
    let jm = find_join_metrics(&metrics);
    assert_eq!(jm.counters.runtime_filter_type.as_deref(), Some("Bloom"));
    assert!(jm.counters.runtime_filter_passed_rows >= 5);
}

// ----------------------------------------------------------------------------
// External Hash Join (R436 - R440)
// ----------------------------------------------------------------------------

#[test]
fn test_r436_external_hash_join() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    // Force external spilling by setting memory budget to 256 bytes
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(256));

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.val = t2.val;")
        .unwrap();

    assert_eq!(res.rows.len(), 100);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.runs_created > 0 || jm.counters.bytes_spilled > 0);
}

#[test]
fn test_r437_partition_local_correctness() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE p (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=120 {
        txn.execute_statement(&format!("INSERT INTO p VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
    }

    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(256));

    let res_filter_on = query(&mut txn, "SELECT * FROM p JOIN b ON p.k = b.k;");

    txn.db_mut().set_join_config(
        JoinConfig::new()
            .with_memory_budget_bytes(256)
            .with_enable_runtime_filter(false),
    );
    let res_filter_off = query(&mut txn, "SELECT * FROM p JOIN b ON p.k = b.k;");

    assert_eq!(res_filter_on.len(), 30);
    assert_eq!(res_filter_on, res_filter_off);
}

#[test]
fn test_r438_global_filter_correctness_in_memory() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, a INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, b INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for &val in &[10, 20, 30] {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({val}, {val});"))
            .unwrap();
    }

    let res = query(
        &mut txn,
        "SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.a = t2.b;",
    );
    assert_eq!(res.len(), 3);
}

#[test]
fn test_r439_spill_interaction() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=80 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(512));

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();

    assert_eq!(res.rows.len(), 80);
    let jm = find_join_metrics(&metrics);
    assert!(jm.counters.partition_passes >= 1);
}

#[test]
fn test_r440_temp_cleanup() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(256));

    let res = query(&mut txn, "SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;");
    assert_eq!(res.len(), 50);
}

// ----------------------------------------------------------------------------
// Outer Join Safety (R441 - R444)
// ----------------------------------------------------------------------------

#[test]
fn test_r441_left_join_remains_correct() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name STRING);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'User_{i}');"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO orders VALUES (1, 2);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (2, 4);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT u.id, o.id FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY u.id;",
    );
    // All 10 users must be present
    assert_eq!(res.len(), 10);
    assert_eq!(res[0].values[1], Value::Null); // User 1 has no order
    assert_eq!(res[1].values[1], Value::Int64(1)); // User 2 has order 1
}

#[test]
fn test_r442_right_join_remains_correct() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name STRING);")
        .unwrap();

    txn.execute_statement("INSERT INTO orders VALUES (1, 2);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (2, 4);")
        .unwrap();
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'User_{i}');"))
            .unwrap();
    }

    let res = query(
        &mut txn,
        "SELECT o.id, u.id FROM orders o RIGHT JOIN users u ON o.user_id = u.id ORDER BY u.id;",
    );
    assert_eq!(res.len(), 5);
}

#[test]
fn test_r443_full_join_remains_correct() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 20);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (2, 30);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k ORDER BY a.id, b.id;",
    );
    assert_eq!(res.len(), 3);
}

#[test]
fn test_r444_runtime_filter_not_incorrectly_applied_to_preserved_side() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE l (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE r (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO r VALUES (1, 5);")
        .unwrap();

    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM l LEFT JOIN r ON l.k = r.k;")
        .unwrap();

    // Must preserve all 20 rows of left table
    assert_eq!(res.rows.len(), 20);
    let jm = find_join_metrics(&metrics);
    // Filter must NOT be enabled on LEFT JOIN
    assert!(!jm.counters.runtime_filter_enabled);
}

// ----------------------------------------------------------------------------
// SEMI / ANTI (R445 - R447)
// ----------------------------------------------------------------------------

#[test]
fn test_r445_semi_semantics_unchanged() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name STRING);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'User_{i}');"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO orders VALUES (1, 2);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (2, 2);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (3, 5);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT u.id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id;",
    );
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].values[0], Value::Int64(2));
    assert_eq!(res[1].values[0], Value::Int64(5));
}

#[test]
fn test_r446_anti_semantics_unchanged() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name STRING);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();

    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'User_{i}');"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO orders VALUES (1, 2);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (2, 4);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT u.id FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id;",
    );
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].values[0], Value::Int64(1));
    assert_eq!(res[1].values[0], Value::Int64(3));
    assert_eq!(res[2].values[0], Value::Int64(5));
}

#[test]
fn test_r447_not_in_null_semantics_unchanged() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (1, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, NULL);")
        .unwrap();

    // NOT IN with NULL in right side produces 0 rows (3VL)
    let res = query(
        &mut txn,
        "SELECT * FROM t1 WHERE val NOT IN (SELECT val FROM t2);",
    );
    assert!(res.is_empty());
}

// ----------------------------------------------------------------------------
// Residual Predicates (R448 - R450)
// ----------------------------------------------------------------------------

#[test]
fn test_r448_residual_equality() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, k INT, val1 INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT, val2 INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 100);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 10, 200);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 10, 100);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.val1 = b.val2;",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
}

#[test]
fn test_r449_residual_range() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, k INT, age INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT, min_age INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 25);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 10, 15);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 10, 18);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT a.id FROM a JOIN b ON a.k = b.k AND a.age >= b.min_age;",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
}

#[test]
fn test_r450_residual_null_predicate() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, k INT, note STRING);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, k INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 'ok');")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 10, NULL);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 10);")
        .unwrap();

    let res = query(
        &mut txn,
        "SELECT a.id FROM a JOIN b ON a.k = b.k AND a.note IS NOT NULL;",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
}

// ----------------------------------------------------------------------------
// EXPLAIN & Metrics (R451 - R453)
// ----------------------------------------------------------------------------

#[test]
fn test_r451_deterministic_explain() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let plan1 = txn
        .explain_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();
    let plan2 = txn
        .explain_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();

    assert_eq!(plan1, plan2);
    assert!(plan1.contains("Runtime Filter:"));
    assert!(plan1.contains("Type: ExactSet"));
}

#[test]
fn test_r452_runtime_filter_plan_metadata() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let plan = txn
        .explain_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();
    assert!(plan.contains("Runtime Filter:"));
    assert!(plan.contains("Scope: Global"));
    assert!(plan.contains("Key: [t2.k]"));
}

#[test]
fn test_r453_explain_analyze_counters() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let res = txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();
    let out = res.rows[0].values[0].to_string();

    assert!(out.contains("Runtime Filter:"));
    assert!(out.contains("build_rows: 5"));
    assert!(out.contains("distinct_keys: 5"));
    assert!(out.contains("probe_rows: 30"));
    assert!(out.contains("passed_rows: 5"));
    assert!(out.contains("rejected_rows: 25"));
}

// ----------------------------------------------------------------------------
// Prepared Statements & Plan Cache (R454 - R458)
// ----------------------------------------------------------------------------

#[test]
fn test_r454_prepared_statement() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let stmt = txn
        .prepare_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t1.id > ?;")
        .unwrap();

    let res1 = txn
        .execute_prepared_query(&stmt, &[Value::Int64(2)])
        .unwrap()
        .rows;
    let res2 = txn
        .execute_prepared_query(&stmt, &[Value::Int64(4)])
        .unwrap()
        .rows;

    assert_eq!(res1.len(), 3); // 3, 4, 5
    assert_eq!(res2.len(), 1); // 5
}

#[test]
fn test_r455_parameter_sensitive_planning() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let stmt = txn
        .prepare_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t2.id = ?;")
        .unwrap();

    let res = txn
        .execute_prepared_query(&stmt, &[Value::Int64(3)])
        .unwrap()
        .rows;
    assert_eq!(res.len(), 1);
}

#[test]
fn test_r456_plan_cache_reuse() {
    let (mut db, _temp) = create_test_db();
    db.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    db.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        db.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();

    let stmt = txn
        .prepare_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t1.id > ?;")
        .unwrap();
    let (_r1, m1) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();
    let (_r2, m2) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();

    assert!(!m1.plan_cache_hit);
    assert!(m2.plan_cache_hit);
}

#[test]
fn test_r457_statistics_invalidation() {
    let (mut db, _temp) = create_test_db();
    db.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    db.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        db.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();

    let stmt = txn
        .prepare_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t1.id > ?;")
        .unwrap();
    let (_r1, _) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();

    // ANALYZE invalidates cached plan
    txn.execute_statement("ANALYZE t1;").unwrap();

    let (_r2, m2) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();
    assert!(!m2.plan_cache_hit);
}

#[test]
fn test_r458_resource_signature_invalidation() {
    let (mut db, _temp) = create_test_db();
    db.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    db.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        db.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();

    let stmt = txn
        .prepare_sql("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t1.id > ?;")
        .unwrap();
    let (_r1, _) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();

    // Changing join config changes resource signature, bypassing stale plan
    txn.db_mut()
        .set_join_config(JoinConfig::new().with_memory_budget_bytes(1024 * 1024));

    let (_r2, m2) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(0)])
        .unwrap();
    assert!(!m2.plan_cache_hit);
}

// ----------------------------------------------------------------------------
// Adaptive Feedback (R459 - R462)
// ----------------------------------------------------------------------------

#[test]
fn test_r459_runtime_filter_feedback() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let (_res, _metrics) = txn
        .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
        .unwrap();

    let registry = txn.db().feedback_registry();
    assert!(!registry.is_empty());
}

#[test]
fn test_r460_feedback_convergence() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    for _ in 0..5 {
        let (res, _m) = txn
            .execute_sql_with_metrics("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;")
            .unwrap();
        assert_eq!(res.rows.len(), 5);
    }
}

#[test]
fn test_r461_bounded_feedback_registry() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    for i in 1..=20 {
        let sql = format!("SELECT * FROM t1 JOIN t2 ON t1.k = t2.k WHERE t1.id > {i};");
        let _ = txn.execute_sql_with_metrics(&sql).unwrap();
    }

    let registry = txn.db().feedback_registry();
    assert!(registry.len() <= treedb::query::feedback::DEFAULT_FEEDBACK_CAPACITY);
}

#[test]
fn test_r462_feedback_does_not_alter_correctness() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
        .unwrap();

    for i in 1..=25 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }

    let ground_truth = query(
        &mut txn,
        "SELECT * FROM t1 JOIN t2 ON t1.k = t2.k ORDER BY t1.id;",
    );

    for _ in 0..10 {
        let res = query(
            &mut txn,
            "SELECT * FROM t1 JOIN t2 ON t1.k = t2.k ORDER BY t1.id;",
        );
        assert_eq!(res, ground_truth);
    }
}

// ----------------------------------------------------------------------------
// DML & Transactions (R463 - R465)
// ----------------------------------------------------------------------------

#[test]
fn test_r463_update_with_join() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE target (id INT PRIMARY KEY, score INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE bonus (id INT PRIMARY KEY, points INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO target VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO target VALUES (2, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO target VALUES (3, 30);")
        .unwrap();

    txn.execute_statement("INSERT INTO bonus VALUES (2, 50);")
        .unwrap();

    txn.execute_statement("UPDATE target SET score = 999 WHERE id IN (SELECT id FROM bonus);")
        .unwrap();

    let res = query(&mut txn, "SELECT id, score FROM target ORDER BY id;");
    assert_eq!(res[0].values[1], Value::Int64(10));
    assert_eq!(res[1].values[1], Value::Int64(999));
    assert_eq!(res[2].values[1], Value::Int64(30));
}

#[test]
fn test_r464_delete_with_join() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE blacklist (id INT PRIMARY KEY);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO blacklist VALUES (3);")
        .unwrap();
    txn.execute_statement("INSERT INTO blacklist VALUES (7);")
        .unwrap();

    txn.execute_statement("DELETE FROM items WHERE id IN (SELECT id FROM blacklist);")
        .unwrap();

    let res = query(&mut txn, "SELECT id FROM items ORDER BY id;");
    assert_eq!(res.len(), 8);
    let ids: Vec<i64> = res
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => 0,
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 4, 5, 6, 8, 9, 10]);
}

#[test]
fn test_r465_rollback_and_recovery_equivalence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("rf_recover.db");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, k INT);")
            .unwrap();
        txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, k INT);")
            .unwrap();

        txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")
            .unwrap();
        txn.execute_statement("INSERT INTO t2 VALUES (1, 10);")
            .unwrap();
        txn.commit().unwrap();

        let mut txn_abort = db.begin_transaction().unwrap();
        txn_abort
            .execute_statement("INSERT INTO t1 VALUES (2, 20);")
            .unwrap();
        txn_abort.abort().unwrap();
    }

    // Reopen database
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin_transaction().unwrap();

        let res = query(&mut txn, "SELECT * FROM t1 JOIN t2 ON t1.k = t2.k;");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].values[0], Value::Int64(1));
    }
}

// ----------------------------------------------------------------------------
// Randomized Differential Oracle Test
// ----------------------------------------------------------------------------

#[test]
fn test_randomized_runtime_filter_differential_oracle() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE left_tbl (id INT PRIMARY KEY, k1 INT, k2 INT, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE right_tbl (id INT PRIMARY KEY, k1 INT, k2 INT, val INT);")
        .unwrap();

    // Deterministic pseudo-random sequence
    let mut seed = 0x12345678u64;
    let mut next_rand = || -> u64 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed >> 33
    };

    for i in 1..=60 {
        let k1_val = if next_rand() % 5 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", next_rand() % 15)
        };
        let k2_val = if next_rand() % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", next_rand() % 10)
        };
        let val = next_rand() % 100;
        txn.execute_statement(&format!(
            "INSERT INTO left_tbl VALUES ({i}, {k1_val}, {k2_val}, {val});"
        ))
        .unwrap();
    }

    for i in 1..=25 {
        let k1_val = if next_rand() % 4 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", next_rand() % 15)
        };
        let k2_val = if next_rand() % 6 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", next_rand() % 10)
        };
        let val = next_rand() % 100;
        txn.execute_statement(&format!(
            "INSERT INTO right_tbl VALUES ({i}, {k1_val}, {k2_val}, {val});"
        ))
        .unwrap();
    }

    let queries = [
        "SELECT l.id, r.id FROM left_tbl l JOIN right_tbl r ON l.k1 = r.k1 ORDER BY l.id, r.id;",
        "SELECT l.id, r.id FROM left_tbl l JOIN right_tbl r ON l.k1 = r.k1 AND l.k2 = r.k2 ORDER BY l.id, r.id;",
        "SELECT l.id, r.id FROM left_tbl l JOIN right_tbl r ON l.k1 = r.k1 AND l.val > r.val ORDER BY l.id, r.id;",
        "SELECT l.id, r.id FROM left_tbl l LEFT JOIN right_tbl r ON l.k1 = r.k1 ORDER BY l.id, r.id;",
        "SELECT l.id, r.id FROM left_tbl l RIGHT JOIN right_tbl r ON l.k1 = r.k1 ORDER BY l.id, r.id;",
        "SELECT l.id, r.id FROM left_tbl l FULL JOIN right_tbl r ON l.k1 = r.k1 ORDER BY l.id, r.id;",
    ];

    for &q in &queries {
        txn.db_mut()
            .set_join_config(JoinConfig::new().with_enable_runtime_filter(true));
        let res_enabled = query(&mut txn, q);

        txn.db_mut()
            .set_join_config(JoinConfig::new().with_enable_runtime_filter(false));
        let res_disabled = query(&mut txn, q);

        assert_eq!(
            res_enabled, res_disabled,
            "Mismatch between runtime filter enabled vs disabled for query: {q}"
        );
    }
}
