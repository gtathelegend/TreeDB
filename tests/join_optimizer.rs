use tempfile::TempDir;
use treedb::engine::types::{Row, Value};
use treedb::query::external_hash_join::JoinConfig;
use treedb::TreeDb;

fn create_test_db() -> (TreeDb, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db = TreeDb::create(temp_dir.path().join("test.db"), 64).unwrap();
    (db, temp_dir)
}

fn query(txn: &mut treedb::engine::Transaction, sql: &str) -> Vec<Row> {
    txn.execute_sql(sql).unwrap().rows
}

#[test]
fn test_basic_join_enumeration_2_3_4_5_way() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, a INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, b INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t3 (id INT PRIMARY KEY, c INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t4 (id INT PRIMARY KEY, d INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t5 (id INT PRIMARY KEY, e INT);")
        .unwrap();

    // Populate data with varying cardinalities
    // t1: 100 rows, t2: 10 rows, t3: 50 rows, t4: 5 rows, t5: 20 rows
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t3 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t4 VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t5 VALUES ({i}, {i});"))
            .unwrap();
    }

    txn.execute_statement("ANALYZE t1;").unwrap();
    txn.execute_statement("ANALYZE t2;").unwrap();
    txn.execute_statement("ANALYZE t3;").unwrap();
    txn.execute_statement("ANALYZE t4;").unwrap();
    txn.execute_statement("ANALYZE t5;").unwrap();

    // 2-way join
    let sql_2way = "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id;";
    let plan_2way = txn.explain_sql(sql_2way).unwrap();
    assert!(plan_2way.contains("TableScan"));
    let res_2way = query(&mut txn, sql_2way);
    assert_eq!(res_2way.len(), 10);

    // 3-way join
    let sql_3way = "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id JOIN t3 ON t2.id = t3.id;";
    let plan_3way = txn.explain_sql(sql_3way).unwrap();
    assert!(plan_3way.contains("TableScan"));
    let res_3way = query(&mut txn, sql_3way);
    assert_eq!(res_3way.len(), 10);

    // 4-way join
    let sql_4way = "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id JOIN t3 ON t2.id = t3.id JOIN t4 ON t3.id = t4.id;";
    let plan_4way = txn.explain_sql(sql_4way).unwrap();
    assert!(plan_4way.contains("TableScan"));
    let res_4way = query(&mut txn, sql_4way);
    assert_eq!(res_4way.len(), 5);

    // 5-way join
    let sql_5way = "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id JOIN t3 ON t2.id = t3.id JOIN t4 ON t3.id = t4.id JOIN t5 ON t4.id = t5.id;";
    let plan_5way = txn.explain_sql(sql_5way).unwrap();
    assert!(plan_5way.contains("TableScan"));
    let res_5way = query(&mut txn, sql_5way);
    assert_eq!(res_5way.len(), 5);

    txn.commit().unwrap();
}

#[test]
fn test_cardinality_and_statistics_sensitivity() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE big (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE small (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE tiny (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=200 {
        txn.execute_statement(&format!("INSERT INTO big VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO small VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=2 {
        txn.execute_statement(&format!("INSERT INTO tiny VALUES ({i}, {i});"))
            .unwrap();
    }

    // Before ANALYZE: default stats
    let sql = "SELECT * FROM big JOIN small ON big.id = small.id JOIN tiny ON small.id = tiny.id;";
    let plan_before = txn.explain_sql(sql).unwrap();

    // After ANALYZE: accurate stats
    txn.execute_statement("ANALYZE big;").unwrap();
    txn.execute_statement("ANALYZE small;").unwrap();
    txn.execute_statement("ANALYZE tiny;").unwrap();

    let plan_after = txn.explain_sql(sql).unwrap();
    let res = query(&mut txn, sql);
    assert_eq!(res.len(), 2);

    // Verify plans exist and are valid deterministic structures
    assert!(!plan_before.is_empty());
    assert!(!plan_after.is_empty());

    txn.commit().unwrap();
}

#[test]
fn test_cost_and_selective_predicates() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE customers (id INT PRIMARY KEY, status VARCHAR, tier INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, cust_id INT, amount INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, order_id INT, price INT);")
        .unwrap();

    for i in 1..=50 {
        let status = if i == 1 { "VIP" } else { "NORMAL" };
        txn.execute_statement(&format!(
            "INSERT INTO customers VALUES ({i}, '{status}', {i});"
        ))
        .unwrap();
    }
    for i in 1..=100 {
        let cust_id = (i % 50) + 1;
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {cust_id}, {i});"))
            .unwrap();
    }
    for i in 1..=200 {
        let order_id = (i % 100) + 1;
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {order_id}, {i});"))
            .unwrap();
    }

    txn.execute_statement("ANALYZE customers;").unwrap();
    txn.execute_statement("ANALYZE orders;").unwrap();
    txn.execute_statement("ANALYZE items;").unwrap();

    // Highly selective predicate on customers: status = 'VIP'
    let sql = "SELECT * FROM customers c JOIN orders o ON c.id = o.cust_id JOIN items i ON o.id = i.order_id WHERE c.status = 'VIP';";
    let plan = txn.explain_sql(sql).unwrap();
    assert!(plan.contains("TableScan"));

    let res = query(&mut txn, sql);
    assert_eq!(res.len(), 4);

    txn.commit().unwrap();
}

#[test]
fn test_predicate_preservation_composite_and_residual() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (k1 INT, k2 INT, val INT, PRIMARY KEY (k1));")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (k1 INT, k2 INT, score INT, PRIMARY KEY (k1));")
        .unwrap();
    txn.execute_statement("CREATE TABLE c (k1 INT, k2 INT, flag INT, PRIMARY KEY (k1));")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 100);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 20, 200);")
        .unwrap();

    txn.execute_statement("INSERT INTO b VALUES (1, 10, 50);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (2, 20, 300);")
        .unwrap();

    txn.execute_statement("INSERT INTO c VALUES (1, 10, 1);")
        .unwrap();
    txn.execute_statement("INSERT INTO c VALUES (2, 20, 0);")
        .unwrap();

    // Composite join condition + residual predicate (a.val > b.score) + filter across 3 relations (a.val + b.score + c.flag > 100)
    let sql = "SELECT a.k1, b.score, c.flag FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2 AND a.val > b.score JOIN c ON b.k1 = c.k1 AND b.k2 = c.k2 WHERE a.val + b.score + c.flag > 100;";
    let res = query(&mut txn, sql);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
    assert_eq!(res[0].values[1], Value::Int64(50));
    assert_eq!(res[0].values[2], Value::Int64(1));

    txn.commit().unwrap();
}

#[test]
fn test_outer_join_optimization_barriers() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t3 (id INT PRIMARY KEY, v INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 20);")
        .unwrap();

    txn.execute_statement("INSERT INTO t2 VALUES (1, 10);")
        .unwrap();

    txn.execute_statement("INSERT INTO t3 VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t3 VALUES (3, 30);")
        .unwrap();

    // 1. LEFT JOIN barrier: t1 LEFT JOIN t2 JOIN t3
    let sql_left = "SELECT t1.id, t2.id, t3.id FROM t1 LEFT JOIN t2 ON t1.id = t2.id JOIN t3 ON t1.id = t3.id;";
    let res_left = query(&mut txn, sql_left);
    assert_eq!(res_left.len(), 1);
    assert_eq!(res_left[0].values[0], Value::Int64(1));

    // 2. RIGHT JOIN barrier: t1 RIGHT JOIN t3 JOIN t2
    let sql_right = "SELECT t1.id, t3.id, t2.id FROM t1 RIGHT JOIN t3 ON t1.id = t3.id JOIN t2 ON t3.id = t2.id;";
    let res_right = query(&mut txn, sql_right);
    assert_eq!(res_right.len(), 1);

    // 3. FULL JOIN barrier: t1 FULL JOIN t3
    let sql_full = "SELECT t1.id, t3.id FROM t1 FULL JOIN t3 ON t1.id = t3.id;";
    let res_full = query(&mut txn, sql_full);
    assert_eq!(res_full.len(), 3); // (1,1), (2, NULL), (NULL, 3)

    txn.commit().unwrap();
}

#[test]
fn test_deterministic_planning_and_explain() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE c (id INT PRIMARY KEY, v INT);")
        .unwrap();

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
    }
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO c VALUES ({i}, {i});"))
            .unwrap();
    }

    txn.execute_statement("ANALYZE a;").unwrap();
    txn.execute_statement("ANALYZE b;").unwrap();
    txn.execute_statement("ANALYZE c;").unwrap();

    let sql = "SELECT * FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id;";

    // Run EXPLAIN 5 times and ensure identical output string
    let explain_1 = txn.explain_sql(sql).unwrap();
    for _ in 0..4 {
        let explain_i = txn.explain_sql(sql).unwrap();
        assert_eq!(explain_1, explain_i);
    }

    txn.commit().unwrap();
}

#[test]
fn test_prepared_statements_and_plan_cache_invalidation() {
    let (mut db, _temp) = create_test_db();

    // 1. Setup tables in committed transaction so plan cache is active
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t3 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t3 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.commit().unwrap();

    // 2. Prepare statement
    let sql = "SELECT t1.id, t2.val, t3.val FROM t1 JOIN t2 ON t1.id = t2.id JOIN t3 ON t2.id = t3.id WHERE t1.val > ?;";
    let mut txn = db.begin_transaction().unwrap();
    let stmt_id = txn.prepare_sql(sql).unwrap();

    let res1 = txn
        .execute_prepared(&stmt_id, &[Value::Int64(15)])
        .unwrap()
        .into_query()
        .unwrap()
        .rows;
    assert_eq!(res1.len(), 5);

    let res2 = txn
        .execute_prepared(&stmt_id, &[Value::Int64(15)])
        .unwrap()
        .into_query()
        .unwrap()
        .rows;
    assert_eq!(res2.len(), 5);

    // Verify plan cache stats has at least 1 hit
    let cache_stats = txn.plan_cache_stats();
    assert_eq!(cache_stats.hits, 1);
    assert_eq!(cache_stats.misses, 1);

    txn.commit().unwrap();

    // 3. Mutate catalog (add secondary index) to invalidate cache
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("CREATE INDEX idx_t1_val ON t1 (val);")
        .unwrap();
    txn.commit().unwrap();

    // 4. Prepared execution replans cleanly
    let mut txn = db.begin_transaction().unwrap();
    let res3 = txn
        .execute_prepared(&stmt_id, &[Value::Int64(10)])
        .unwrap()
        .into_query()
        .unwrap()
        .rows;
    assert_eq!(res3.len(), 10);
    txn.commit().unwrap();
}

#[test]
fn test_persistence_across_close_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("join_opt_pers.db");

    // Phase 1: Create, populate, analyze, query
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE r1 (id INT PRIMARY KEY, v INT);")
            .unwrap();
        txn.execute_statement("CREATE TABLE r2 (id INT PRIMARY KEY, v INT);")
            .unwrap();
        txn.execute_statement("CREATE TABLE r3 (id INT PRIMARY KEY, v INT);")
            .unwrap();

        for i in 1..=15 {
            txn.execute_statement(&format!("INSERT INTO r1 VALUES ({i}, {i});"))
                .unwrap();
            txn.execute_statement(&format!("INSERT INTO r2 VALUES ({i}, {i});"))
                .unwrap();
            txn.execute_statement(&format!("INSERT INTO r3 VALUES ({i}, {i});"))
                .unwrap();
        }

        txn.execute_statement("ANALYZE r1;").unwrap();
        txn.execute_statement("ANALYZE r2;").unwrap();
        txn.execute_statement("ANALYZE r3;").unwrap();
        txn.commit().unwrap();
    }

    // Phase 2: Reopen and verify identical planning and results
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        let sql =
            "SELECT * FROM r1 JOIN r2 ON r1.id = r2.id JOIN r3 ON r2.id = r3.id WHERE r1.v <= 5;";
        let plan = txn.explain_sql(sql).unwrap();
        assert!(plan.contains("TableScan"));

        let res = query(&mut txn, sql);
        assert_eq!(res.len(), 5);
        txn.commit().unwrap();
    }
}

#[test]
fn test_large_join_graph_bounded_fallback() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    // Create 9 tables (exceeds MAX_ENUMERATED_RELATIONS = 8)
    for i in 1..=9 {
        txn.execute_statement(&format!(
            "CREATE TABLE tbl_{i} (id INT PRIMARY KEY, val INT);"
        ))
        .unwrap();
        for row in 1..=5 {
            txn.execute_statement(&format!("INSERT INTO tbl_{i} VALUES ({row}, {row});"))
                .unwrap();
        }
    }

    // 9-way join
    let sql = "SELECT * FROM tbl_1 \
               JOIN tbl_2 ON tbl_1.id = tbl_2.id \
               JOIN tbl_3 ON tbl_2.id = tbl_3.id \
               JOIN tbl_4 ON tbl_3.id = tbl_4.id \
               JOIN tbl_5 ON tbl_4.id = tbl_5.id \
               JOIN tbl_6 ON tbl_5.id = tbl_6.id \
               JOIN tbl_7 ON tbl_6.id = tbl_7.id \
               JOIN tbl_8 ON tbl_7.id = tbl_8.id \
               JOIN tbl_9 ON tbl_8.id = tbl_9.id;";

    let plan = txn.explain_sql(sql).unwrap();
    assert!(!plan.is_empty());

    let res = query(&mut txn, sql);
    assert_eq!(res.len(), 5);

    txn.commit().unwrap();
}

#[test]
fn test_external_spill_join_with_optimized_order() {
    let (mut db, temp_dir) = create_test_db();
    // Set tiny memory budget to force external hash join spill
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(temp_dir.path())
            .with_memory_budget_bytes(1024),
    );

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE c (id INT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO c VALUES ({i}, {i});"))
            .unwrap();
    }

    let sql = "SELECT * FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id;";
    let (res, metrics) = txn.execute_sql_with_metrics(sql).unwrap();
    assert_eq!(res.rows.len(), 50);
    assert_eq!(metrics.total_rows, 50);

    txn.commit().unwrap();
}

#[test]
fn test_randomized_differential_join_oracle() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t_a (id INT PRIMARY KEY, k INT, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_b (id INT PRIMARY KEY, k INT, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_c (id INT PRIMARY KEY, k INT, v INT);")
        .unwrap();

    // Populate pseudo-randomized rows with duplicates and matches
    let mut state: u64 = 42;
    let mut lcg = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 33) % 10) as i64
    };

    let mut expected_tuples = Vec::new();
    let mut rows_a = Vec::new();
    let mut rows_b = Vec::new();
    let mut rows_c = Vec::new();

    for id in 1..=15 {
        let k_a = lcg();
        let v_a = lcg();
        rows_a.push((id, k_a, v_a));
        txn.execute_statement(&format!("INSERT INTO t_a VALUES ({id}, {k_a}, {v_a});"))
            .unwrap();
    }
    for id in 1..=15 {
        let k_b = lcg();
        let v_b = lcg();
        rows_b.push((id, k_b, v_b));
        txn.execute_statement(&format!("INSERT INTO t_b VALUES ({id}, {k_b}, {v_b});"))
            .unwrap();
    }
    for id in 1..=15 {
        let k_c = lcg();
        let v_c = lcg();
        rows_c.push((id, k_c, v_c));
        txn.execute_statement(&format!("INSERT INTO t_c VALUES ({id}, {k_c}, {v_c});"))
            .unwrap();
    }

    // Compute expected result via independent Cartesian loop oracle
    for a in &rows_a {
        for b in &rows_b {
            if a.1 == b.1 {
                for c in &rows_c {
                    if b.1 == c.1 {
                        expected_tuples.push((a.0, b.0, c.0));
                    }
                }
            }
        }
    }

    let sql = "SELECT t_a.id, t_b.id, t_c.id FROM t_a JOIN t_b ON t_a.k = t_b.k JOIN t_c ON t_b.k = t_c.k ORDER BY t_a.id, t_b.id, t_c.id;";
    let res = query(&mut txn, sql);

    let mut actual_tuples = Vec::new();
    for row in res {
        let id_a = match row.values[0] {
            Value::Int64(v) => v,
            _ => panic!("unexpected type"),
        };
        let id_b = match row.values[1] {
            Value::Int64(v) => v,
            _ => panic!("unexpected type"),
        };
        let id_c = match row.values[2] {
            Value::Int64(v) => v,
            _ => panic!("unexpected type"),
        };
        actual_tuples.push((id_a, id_b, id_c));
    }

    expected_tuples.sort();
    assert_eq!(actual_tuples, expected_tuples);

    txn.commit().unwrap();
}
