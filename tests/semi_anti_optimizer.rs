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

fn explain(txn: &mut treedb::engine::Transaction, sql: &str) -> String {
    let res = txn.execute_sql(&format!("EXPLAIN {sql}")).unwrap();
    match &res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("Expected string from EXPLAIN"),
    }
}

fn explain_analyze(txn: &mut treedb::engine::Transaction, sql: &str) -> (Vec<Row>, String) {
    let (res, _metrics) = txn.execute_sql_with_metrics(sql).unwrap();
    let ea_res = txn.execute_sql(&format!("EXPLAIN ANALYZE {sql}")).unwrap();
    let plan_text = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("Expected string from EXPLAIN ANALYZE"),
    };
    (res.rows, plan_text)
}

// -----------------------------------------------------------------------------
// 1. Basic SEMI/ANTI Join Semantics & Duplicate Elimination
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_basic_semantics_and_duplicate_elimination() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT, city TEXT);")
        .unwrap();
    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT, amount BIGINT);",
    )
    .unwrap();

    // Customers: 1, 2, 3, 4, 5
    txn.execute_statement("INSERT INTO customers VALUES (1, 'Alice', 'NYC');")
        .unwrap();
    txn.execute_statement("INSERT INTO customers VALUES (2, 'Bob', 'LA');")
        .unwrap();
    txn.execute_statement("INSERT INTO customers VALUES (3, 'Charlie', 'Chicago');")
        .unwrap();
    txn.execute_statement("INSERT INTO customers VALUES (4, 'David', 'Miami');")
        .unwrap();
    txn.execute_statement("INSERT INTO customers VALUES (5, 'Eve', 'Seattle');")
        .unwrap();

    // Orders: Customer 1 has 3 orders, Customer 2 has 2 orders, Customer 3 has 0 orders, Customer 4 has 1 order
    txn.execute_statement("INSERT INTO orders VALUES (101, 1, 50);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (102, 1, 100);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (103, 1, 150);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (104, 2, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (105, 2, 40);")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (106, 4, 300);")
        .unwrap();

    // SEMI JOIN via EXISTS: Customer 1 must appear EXACTLY ONCE (duplicate elimination)
    let res = query(
        &mut txn,
        "SELECT id, name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id) ORDER BY id;",
    );
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].values[0], Value::Int64(1));
    assert_eq!(res[1].values[0], Value::Int64(2));
    assert_eq!(res[2].values[0], Value::Int64(4));

    // ANTI JOIN via NOT EXISTS: Customers 3 and 5 have no orders
    let res_anti = query(
        &mut txn,
        "SELECT id, name FROM customers c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id) ORDER BY id;",
    );
    assert_eq!(res_anti.len(), 2);
    assert_eq!(res_anti[0].values[0], Value::Int64(3));
    assert_eq!(res_anti[1].values[0], Value::Int64(5));

    // IN (subquery) decorrelated to SEMI join
    let res_in = query(
        &mut txn,
        "SELECT id, name FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o) ORDER BY id;",
    );
    assert_eq!(res_in.len(), 3);
    assert_eq!(res_in[0].values[0], Value::Int64(1));
    assert_eq!(res_in[1].values[0], Value::Int64(2));
    assert_eq!(res_in[2].values[0], Value::Int64(4));
}

#[test]
fn test_semi_anti_null_and_empty_edge_cases() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t_outer (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_inner (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    // t_outer has rows with NULL val
    txn.execute_statement("INSERT INTO t_outer VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t_outer VALUES (2, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO t_outer VALUES (3, 20);")
        .unwrap();

    // t_inner has matching row for 10 and another NULL val
    txn.execute_statement("INSERT INTO t_inner VALUES (101, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t_inner VALUES (102, NULL);")
        .unwrap();

    // SEMI JOIN: NULL val in outer does NOT match NULL val in inner (SQL 3VL false/unknown)
    let res = query(
        &mut txn,
        "SELECT id, val FROM t_outer o WHERE EXISTS (SELECT 1 FROM t_inner i WHERE i.val = o.val);",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));

    // ANTI JOIN: NULL val in outer has no match, so it IS retained!
    let res_anti = query(
        &mut txn,
        "SELECT id, val FROM t_outer o WHERE NOT EXISTS (SELECT 1 FROM t_inner i WHERE i.val = o.val) ORDER BY id;",
    );
    assert_eq!(res_anti.len(), 2);
    assert_eq!(res_anti[0].values[0], Value::Int64(2));
    assert_eq!(res_anti[1].values[0], Value::Int64(3));

    // Empty inner table: SEMI returns 0, ANTI returns all outer rows
    txn.execute_statement("CREATE TABLE empty_inner (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    let res_empty_semi = query(
        &mut txn,
        "SELECT id FROM t_outer o WHERE EXISTS (SELECT 1 FROM empty_inner e WHERE e.val = o.val);",
    );
    assert_eq!(res_empty_semi.len(), 0);

    let res_empty_anti = query(
        &mut txn,
        "SELECT id FROM t_outer o WHERE NOT EXISTS (SELECT 1 FROM empty_inner e WHERE e.val = o.val);",
    );
    assert_eq!(res_empty_anti.len(), 3);

    // Empty outer table: both return 0
    txn.execute_statement("CREATE TABLE empty_outer (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    let res_empty_out_semi = query(
        &mut txn,
        "SELECT id FROM empty_outer e WHERE EXISTS (SELECT 1 FROM t_inner i WHERE i.val = e.val);",
    );
    assert_eq!(res_empty_out_semi.len(), 0);

    let res_empty_out_anti = query(
        &mut txn,
        "SELECT id FROM empty_outer e WHERE NOT EXISTS (SELECT 1 FROM t_inner i WHERE i.val = e.val);",
    );
    assert_eq!(res_empty_out_anti.len(), 0);
}

// -----------------------------------------------------------------------------
// 2. Physical Strategy Selection & EXPLAIN Formatting
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_strategy_index_probe_primary() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO customers VALUES ({i}, 'Cust_{i}');"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE orders;").unwrap();
    txn.execute_statement("ANALYZE customers;").unwrap();

    // Query: orders EXISTS customers on customers.id = orders.customer_id
    // customers.id is PRIMARY KEY -> IndexProbe with index=__primary__
    let explain_res = explain(
        &mut txn,
        "SELECT id FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id);",
    );

    assert!(
        explain_res.contains("strategy=IndexProbe"),
        "Expected strategy=IndexProbe in EXPLAIN:\n{explain_res}"
    );
    assert!(
        explain_res.contains("index=__primary__"),
        "Expected index=__primary__ in EXPLAIN:\n{explain_res}"
    );

    // Verify EXPLAIN ANALYZE metrics
    let (rows, analyze_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id);",
    );
    assert_eq!(rows.len(), 50);

    assert!(
        analyze_str.contains("IndexSemiProbe"),
        "Expected IndexSemiProbe in EXPLAIN ANALYZE:\n{analyze_str}"
    );
    assert!(
        analyze_str.contains("index_probes:"),
        "Expected index_probes metric in EXPLAIN ANALYZE:\n{analyze_str}"
    );
    assert!(
        analyze_str.contains("matched_outer_rows:"),
        "Expected matched_outer_rows in EXPLAIN ANALYZE:\n{analyze_str}"
    );
}

#[test]
fn test_semi_anti_strategy_index_probe_secondary() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_orders_customer ON orders(customer_id);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO customers VALUES ({i}, 'Cust_{i}');"))
            .unwrap();
    }
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE customers;").unwrap();
    txn.execute_statement("ANALYZE orders;").unwrap();

    // Query: customers EXISTS orders on orders.customer_id = customers.id
    // orders has secondary index on customer_id -> IndexProbe with index=idx_orders_customer
    let explain_res = explain(
        &mut txn,
        "SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id);",
    );

    assert!(
        explain_res.contains("strategy=IndexProbe"),
        "Expected strategy=IndexProbe in EXPLAIN:\n{explain_res}"
    );
    assert!(
        explain_res.contains("index=idx_orders_customer"),
        "Expected index=idx_orders_customer in EXPLAIN:\n{explain_res}"
    );

    // Verify execution results and EXPLAIN ANALYZE
    let (rows, analyze_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id);",
    );
    assert_eq!(rows.len(), 50);

    assert!(analyze_str.contains("IndexSemiProbe"));
    assert!(analyze_str.contains("successful_probes: 50"));
    assert!(analyze_str.contains("failed_probes: 50"));
}

#[test]
fn test_semi_anti_strategy_hash_for_unindexed_inner() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t_outer (id BIGINT PRIMARY KEY, key_val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_inner (id BIGINT PRIMARY KEY, key_val BIGINT);")
        .unwrap();

    // Large unindexed inner table: 200 rows
    for i in 1..=200 {
        txn.execute_statement(&format!("INSERT INTO t_inner VALUES ({i}, {i});"))
            .unwrap();
    }
    // Outer table: 50 rows
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t_outer VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t_outer;").unwrap();
    txn.execute_statement("ANALYZE t_inner;").unwrap();

    // Without an index on key_val, Hash Semi Join should be chosen over O(N * M) nested loop
    let explain_res = explain(
        &mut txn,
        "SELECT id FROM t_outer o WHERE EXISTS (SELECT 1 FROM t_inner i WHERE i.key_val = o.key_val);",
    );

    assert!(
        explain_res.contains("strategy=Hash"),
        "Expected strategy=Hash in EXPLAIN:\n{explain_res}"
    );

    let (rows, analyze_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM t_outer o WHERE EXISTS (SELECT 1 FROM t_inner i WHERE i.key_val = o.key_val);",
    );
    assert_eq!(rows.len(), 50);

    assert!(analyze_str.contains("HashSemiJoin"));
    assert!(analyze_str.contains("hash_build_rows:"));
    assert!(analyze_str.contains("hash_probe_rows:"));
}

#[test]
fn test_semi_anti_strategy_nested_loop_for_tiny_relations() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE small_outer (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE small_inner (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    // 2 rows outer, 2 rows inner, no index on val
    txn.execute_statement("INSERT INTO small_outer VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO small_outer VALUES (2, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO small_inner VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO small_inner VALUES (2, 30);")
        .unwrap();

    txn.execute_statement("ANALYZE small_outer;").unwrap();
    txn.execute_statement("ANALYZE small_inner;").unwrap();

    let explain_res = explain(
        &mut txn,
        "SELECT id FROM small_outer o WHERE EXISTS (SELECT 1 FROM small_inner i WHERE i.val = o.val);",
    );

    // For tiny tables (2 rows), NestedLoop is cheapest
    assert!(
        explain_res.contains("strategy=NestedLoop"),
        "Expected strategy=NestedLoop in EXPLAIN:\n{explain_res}"
    );

    let (rows, analyze_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM small_outer o WHERE EXISTS (SELECT 1 FROM small_inner i WHERE i.val = o.val);",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));

    assert!(analyze_str.contains("NestedLoopSemiJoin"));
}

// -----------------------------------------------------------------------------
// 3. Statistics Sensitivity & Strategy Transitions
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_statistics_sensitivity_and_index_transition() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE parent (id BIGINT PRIMARY KEY, tag BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE child (id BIGINT PRIMARY KEY, tag BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO parent VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO child VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE parent;").unwrap();
    txn.execute_statement("ANALYZE child;").unwrap();

    // Initially unindexed: Hash strategy
    let explain_before = explain(
        &mut txn,
        "SELECT id FROM parent p WHERE EXISTS (SELECT 1 FROM child c WHERE c.tag = p.tag);",
    );
    assert!(explain_before.contains("strategy=Hash"));

    // Now add secondary index on child(tag)
    txn.execute_statement("CREATE INDEX idx_child_tag ON child(tag);")
        .unwrap();

    // Optimizer should now transition to IndexProbe!
    let explain_after = explain(
        &mut txn,
        "SELECT id FROM parent p WHERE EXISTS (SELECT 1 FROM child c WHERE c.tag = p.tag);",
    );
    assert!(
        explain_after.contains("strategy=IndexProbe"),
        "Expected strategy=IndexProbe after CREATE INDEX:\n{explain_after}"
    );
    assert!(explain_after.contains("index=idx_child_tag"));

    // Now drop the index: optimizer should fall back to Hash!
    txn.execute_statement("DROP INDEX idx_child_tag;").unwrap();
    let explain_drop = explain(
        &mut txn,
        "SELECT id FROM parent p WHERE EXISTS (SELECT 1 FROM child c WHERE c.tag = p.tag);",
    );
    assert!(
        explain_drop.contains("strategy=Hash"),
        "Expected strategy=Hash after DROP INDEX:\n{explain_drop}"
    );
}

// -----------------------------------------------------------------------------
// 4. Residual Predicates & Early Termination
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_residual_predicates_and_early_termination() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE accounts (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement(
        "CREATE TABLE transfers (id BIGINT PRIMARY KEY, account_id BIGINT, amount BIGINT);",
    )
    .unwrap();
    txn.execute_statement("CREATE INDEX idx_transfers_acc ON transfers(account_id);")
        .unwrap();

    txn.execute_statement("INSERT INTO accounts VALUES (1, 'Alice');")
        .unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (2, 'Bob');")
        .unwrap();

    // Account 1 has 3 transfers: 10, 20, 200
    txn.execute_statement("INSERT INTO transfers VALUES (1, 1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO transfers VALUES (2, 1, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO transfers VALUES (3, 1, 200);")
        .unwrap();

    // Account 2 has 2 transfers: 15, 30 (none > 100)
    txn.execute_statement("INSERT INTO transfers VALUES (4, 2, 15);")
        .unwrap();
    txn.execute_statement("INSERT INTO transfers VALUES (5, 2, 30);")
        .unwrap();

    // Query with residual filter: amount > 100
    let res = query(
        &mut txn,
        "SELECT id, name FROM accounts a WHERE EXISTS (SELECT 1 FROM transfers t WHERE t.account_id = a.id AND t.amount > 100);",
    );
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));

    // ANTI with residual filter: Bob has no transfer > 100, so Bob IS returned
    let res_anti = query(
        &mut txn,
        "SELECT id, name FROM accounts a WHERE NOT EXISTS (SELECT 1 FROM transfers t WHERE t.account_id = a.id AND t.amount > 100);",
    );
    assert_eq!(res_anti.len(), 1);
    assert_eq!(res_anti[0].values[0], Value::Int64(2));
}

// -----------------------------------------------------------------------------
// 5. Optimizer Interactions: Multi-Join, Multiple SEMI/ANTI, Barriers
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_multi_join_and_barriers() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, dept_id BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE depts (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE projects (id BIGINT PRIMARY KEY, leader_id BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE audits (id BIGINT PRIMARY KEY, user_id BIGINT);")
        .unwrap();

    txn.execute_statement("INSERT INTO depts VALUES (10, 'Engineering');")
        .unwrap();
    txn.execute_statement("INSERT INTO depts VALUES (20, 'Sales');")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (2, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (3, 20);")
        .unwrap();

    // User 1 leads project 100
    txn.execute_statement("INSERT INTO projects VALUES (100, 1);")
        .unwrap();
    // User 2 has audit record
    txn.execute_statement("INSERT INTO audits VALUES (500, 2);")
        .unwrap();

    // Query combines INNER JOIN with SEMI JOIN (EXISTS) and ANTI JOIN (NOT EXISTS)
    let sql = "SELECT u.id, d.name FROM users u \
               INNER JOIN depts d ON u.dept_id = d.id \
               WHERE EXISTS (SELECT 1 FROM projects p WHERE p.leader_id = u.id) \
               AND NOT EXISTS (SELECT 1 FROM audits a WHERE a.user_id = u.id);";

    let res = query(&mut txn, sql);
    // User 1: in Eng (10), leads project (100), has NO audit -> MATCH
    // User 2: in Eng (10), leads NO project -> rejected
    // User 3: in Sales (20), leads NO project -> rejected
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[0], Value::Int64(1));
    assert_eq!(res[0].values[1], Value::String("Engineering".into()));
}

// -----------------------------------------------------------------------------
// 6. Prepared Statements & Plan Cache Invalidation
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_prepared_statements_and_cache_invalidation() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE items (id BIGINT PRIMARY KEY, cat_id BIGINT, price BIGINT);",
    )
    .unwrap();
    txn.execute_statement(
        "CREATE TABLE sales (id BIGINT PRIMARY KEY, item_id BIGINT, qty BIGINT);",
    )
    .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {}, {i});", i % 5))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO sales VALUES ({i}, {i}, 1);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE items;").unwrap();
    txn.execute_statement("ANALYZE sales;").unwrap();

    let sql = "SELECT id, price FROM items i WHERE EXISTS (SELECT 1 FROM sales s WHERE s.item_id = i.id) AND price > ? ORDER BY id;";
    let stmt = txn.prepare_sql(sql).unwrap();

    // Execution 1: price > 40
    let res1 = txn
        .execute_prepared_query(&stmt, &[Value::Int64(40)])
        .unwrap();
    assert_eq!(res1.rows.len(), 10);

    // Execution 2: price > 45 (plan cache hit)
    let res2 = txn
        .execute_prepared_query(&stmt, &[Value::Int64(45)])
        .unwrap();
    assert_eq!(res2.rows.len(), 5);

    // Now CREATE INDEX on sales(item_id) -> invalidates plan cache
    txn.execute_statement("CREATE INDEX idx_sales_item ON sales(item_id);")
        .unwrap();

    // Execution 3: should re-plan using IndexProbe
    let res3 = txn
        .execute_prepared_query(&stmt, &[Value::Int64(40)])
        .unwrap();
    assert_eq!(res3.rows.len(), 10);

    let explain_after_idx = explain(
        &mut txn,
        "SELECT id, price FROM items i WHERE EXISTS (SELECT 1 FROM sales s WHERE s.item_id = i.id) AND price > 40;",
    );
    assert!(explain_after_idx.contains("strategy=IndexProbe"));
}

// -----------------------------------------------------------------------------
// 7. DML Integration: UPDATE and DELETE with SEMI/ANTI Subqueries
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_dml_update_and_delete() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE members (id BIGINT PRIMARY KEY, active BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE activity (id BIGINT PRIMARY KEY, member_id BIGINT);")
        .unwrap();

    txn.execute_statement("INSERT INTO members VALUES (1, 0);")
        .unwrap();
    txn.execute_statement("INSERT INTO members VALUES (2, 0);")
        .unwrap();
    txn.execute_statement("INSERT INTO members VALUES (3, 0);")
        .unwrap();

    // Activity for members 1 and 3
    txn.execute_statement("INSERT INTO activity VALUES (10, 1);")
        .unwrap();
    txn.execute_statement("INSERT INTO activity VALUES (20, 3);")
        .unwrap();

    // UPDATE with EXISTS
    txn.execute_statement("UPDATE members SET active = 1 WHERE EXISTS (SELECT 1 FROM activity a WHERE a.member_id = members.id);").unwrap();

    let res = query(&mut txn, "SELECT id, active FROM members ORDER BY id;");
    assert_eq!(res[0].values[1], Value::Int64(1)); // member 1 updated
    assert_eq!(res[1].values[1], Value::Int64(0)); // member 2 unchanged
    assert_eq!(res[2].values[1], Value::Int64(1)); // member 3 updated

    // DELETE with NOT EXISTS: delete members without activity
    txn.execute_statement("DELETE FROM members WHERE NOT EXISTS (SELECT 1 FROM activity a WHERE a.member_id = members.id);").unwrap();

    let res_after_del = query(&mut txn, "SELECT id FROM members ORDER BY id;");
    assert_eq!(res_after_del.len(), 2);
    assert_eq!(res_after_del[0].values[0], Value::Int64(1));
    assert_eq!(res_after_del[1].values[0], Value::Int64(3));
}

// -----------------------------------------------------------------------------
// 8. External Spilled Hash SEMI/ANTI Execution
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_external_spill_hash_join() {
    let temp_dir = TempDir::new().unwrap();
    let mut db = TreeDb::create(temp_dir.path().join("test_spill.db"), 64).unwrap();
    db.set_join_config(
        JoinConfig::new()
            .with_temp_dir(temp_dir.path())
            .with_memory_budget_bytes(256),
    );

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("CREATE TABLE large_outer (id BIGINT PRIMARY KEY, key_val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE large_inner (id BIGINT PRIMARY KEY, key_val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO large_outer VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO large_inner VALUES ({i}, {i});"))
            .unwrap();
    }

    // SEMI join with forced spilling
    let (res_semi, semi_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM large_outer o WHERE EXISTS (SELECT 1 FROM large_inner i WHERE i.key_val = o.key_val);",
    );

    assert_eq!(res_semi.len(), 100);
    assert!(
        semi_str.contains("ExternalHashJoin") || semi_str.contains("bytes_spilled:"),
        "Expected ExternalHashJoin metrics:\n{semi_str}"
    );

    // ANTI join with forced spilling
    let (res_anti, anti_str) = explain_analyze(
        &mut txn,
        "SELECT id FROM large_outer o WHERE NOT EXISTS (SELECT 1 FROM large_inner i WHERE i.key_val = o.key_val);",
    );

    assert_eq!(res_anti.len(), 0);
    assert!(anti_str.contains("ExternalHashJoin") || anti_str.contains("bytes_spilled:"));
}

// -----------------------------------------------------------------------------
// 9. Randomized Differential Oracle
// -----------------------------------------------------------------------------

#[test]
fn test_semi_anti_randomized_differential_oracle() {
    let (mut db, _temp) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE rnd_outer (id BIGINT PRIMARY KEY, k BIGINT, val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE rnd_inner (id BIGINT PRIMARY KEY, k BIGINT, info BIGINT);")
        .unwrap();

    // Pseudo-random deterministic sequence
    let mut state: u64 = 987654321;
    let mut next_rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as u32
    };

    // Populate outer table: 80 rows with k in 1..=40 (or NULL)
    for i in 1..=80 {
        let r = next_rand();
        let k_str = if r % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", (r % 40) + 1)
        };
        let val = (next_rand() % 100) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO rnd_outer VALUES ({i}, {k_str}, {val});"
        ))
        .unwrap();
    }

    // Populate inner table: 60 rows with k in 1..=30 (or NULL)
    for i in 1..=60 {
        let r = next_rand();
        let k_str = if r % 6 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", (r % 30) + 1)
        };
        let info = (next_rand() % 200) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO rnd_inner VALUES ({i}, {k_str}, {info});"
        ))
        .unwrap();
    }

    txn.execute_statement("ANALYZE rnd_outer;").unwrap();
    txn.execute_statement("ANALYZE rnd_inner;").unwrap();

    // Execute SEMI query
    let semi_sql = "SELECT id, k FROM rnd_outer o WHERE EXISTS (SELECT 1 FROM rnd_inner i WHERE i.k = o.k) ORDER BY id;";
    let semi_rows = query(&mut txn, semi_sql);

    // Read full tables into memory to compute oracle truth
    let all_outer = query(&mut txn, "SELECT id, k FROM rnd_outer;");
    let all_inner = query(&mut txn, "SELECT id, k FROM rnd_inner;");

    let mut oracle_semi_ids = Vec::new();
    let mut oracle_anti_ids = Vec::new();

    for o_row in &all_outer {
        let o_id = match &o_row.values[0] {
            Value::Int64(v) => *v,
            _ => panic!("Expected Int64"),
        };
        let o_k = &o_row.values[1];

        let mut has_match = false;
        if !o_k.is_null() {
            for i_row in &all_inner {
                let i_k = &i_row.values[1];
                if !i_k.is_null() && o_k == i_k {
                    has_match = true;
                    break;
                }
            }
        }

        if has_match {
            oracle_semi_ids.push(o_id);
        } else {
            oracle_anti_ids.push(o_id);
        }
    }

    let semi_actual_ids: Vec<i64> = semi_rows
        .into_iter()
        .map(|r| match &r.values[0] {
            Value::Int64(v) => *v,
            _ => panic!("Expected Int64"),
        })
        .collect();
    oracle_semi_ids.sort();
    assert_eq!(
        semi_actual_ids, oracle_semi_ids,
        "SEMI join results did not match oracle truth!"
    );

    // Execute ANTI query
    let anti_sql = "SELECT id, k FROM rnd_outer o WHERE NOT EXISTS (SELECT 1 FROM rnd_inner i WHERE i.k = o.k) ORDER BY id;";
    let anti_rows = query(&mut txn, anti_sql);
    let anti_actual_ids: Vec<i64> = anti_rows
        .into_iter()
        .map(|r| match &r.values[0] {
            Value::Int64(v) => *v,
            _ => panic!("Expected Int64"),
        })
        .collect();
    oracle_anti_ids.sort();
    assert_eq!(
        anti_actual_ids, oracle_anti_ids,
        "ANTI join results did not match oracle truth!"
    );
}
