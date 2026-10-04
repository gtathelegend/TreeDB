use std::path::PathBuf;

use treedb::engine::types::{DataType, Value};
use treedb::error::Error;
use treedb::sql::ast::{ExplainStatement, Statement};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_explain_{}_{}_{}.treedb",
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
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = std::fs::remove_file(&bak);
    }
}

// ============================================================================
// 1. Parser Tests: EXPLAIN statements and syntax validations
// ============================================================================

#[test]
fn test_parser_explain_statements() {
    // 1. Simple SELECT
    let stmt =
        treedb::sql::parse_statement("EXPLAIN SELECT id, name FROM users WHERE id = 1;").unwrap();
    match &stmt {
        Statement::Explain(ExplainStatement { query, .. }) => match query.as_ref() {
            Statement::Select(s) => {
                assert_eq!(s.from.name(), "users");
                assert_eq!(s.projection.len(), 2);
            }
            _ => panic!("expected Statement::Select inside Explain"),
        },
        _ => panic!("expected Statement::Explain"),
    }

    // 2. Case-insensitivity for EXPLAIN keyword
    let stmt2 = treedb::sql::parse_statement("explain select * from t;").unwrap();
    assert!(matches!(stmt2, Statement::Explain(_)));

    let stmt3 = treedb::sql::parse_statement("ExPlAiN select * from t;").unwrap();
    assert!(matches!(stmt3, Statement::Explain(_)));

    // 3. EXPLAIN WITH CTE
    let stmt_cte =
        treedb::sql::parse_statement("EXPLAIN WITH c AS (SELECT val AS x FROM t) SELECT x FROM c;")
            .unwrap();
    assert!(matches!(stmt_cte, Statement::Explain(_)));

    // 4. EXPLAIN WITH RECURSIVE
    let stmt_rec = treedb::sql::parse_statement(
        "EXPLAIN WITH RECURSIVE nums AS (SELECT val AS n FROM t UNION ALL SELECT n + 1 FROM nums WHERE n < 5) SELECT * FROM nums;",
    )
    .unwrap();
    assert!(matches!(stmt_rec, Statement::Explain(_)));

    // 5. EXPLAIN Set Operation
    let stmt_set =
        treedb::sql::parse_statement("EXPLAIN SELECT val FROM t1 UNION ALL SELECT val FROM t2;")
            .unwrap();
    assert!(matches!(stmt_set, Statement::Explain(_)));

    // 6. EXPLAIN Parenthesized query
    let stmt_paren = treedb::sql::parse_statement(
        "EXPLAIN (SELECT val FROM t1) INTERSECT (SELECT val FROM t2);",
    )
    .unwrap();
    assert!(matches!(stmt_paren, Statement::Explain(_)));
}

#[test]
fn test_parser_explain_rejections() {
    // DML rejections
    let err_insert =
        treedb::sql::parse_statement("EXPLAIN INSERT INTO users VALUES (1, 'alice');").unwrap_err();
    assert!(matches!(err_insert, Error::UnsupportedExplainTarget(target) if target == "INSERT"));

    let err_update =
        treedb::sql::parse_statement("EXPLAIN UPDATE users SET name = 'bob' WHERE id = 1;")
            .unwrap_err();
    assert!(matches!(err_update, Error::UnsupportedExplainTarget(target) if target == "UPDATE"));

    let err_delete =
        treedb::sql::parse_statement("EXPLAIN DELETE FROM users WHERE id = 1;").unwrap_err();
    assert!(matches!(err_delete, Error::UnsupportedExplainTarget(target) if target == "DELETE"));

    // DDL rejections
    let err_create_table =
        treedb::sql::parse_statement("EXPLAIN CREATE TABLE t (id BIGINT PRIMARY KEY);")
            .unwrap_err();
    assert!(
        matches!(err_create_table, Error::UnsupportedExplainTarget(target) if target == "CREATE TABLE")
    );

    let err_create_index =
        treedb::sql::parse_statement("EXPLAIN CREATE INDEX idx ON t (a);").unwrap_err();
    assert!(
        matches!(err_create_index, Error::UnsupportedExplainTarget(target) if target == "CREATE INDEX")
    );

    let err_drop_table = treedb::sql::parse_statement("EXPLAIN DROP TABLE t;").unwrap_err();
    assert!(
        matches!(err_drop_table, Error::UnsupportedExplainTarget(target) if target == "DROP TABLE")
    );

    let err_drop_index = treedb::sql::parse_statement("EXPLAIN DROP INDEX idx;").unwrap_err();
    assert!(
        matches!(err_drop_index, Error::UnsupportedExplainTarget(target) if target == "DROP INDEX")
    );

    // Malformed EXPLAIN statements
    let err_empty = treedb::sql::parse_statement("EXPLAIN;").unwrap_err();
    assert!(matches!(err_empty, Error::SqlParseError { .. }));

    let err_eof = treedb::sql::parse_statement("EXPLAIN").unwrap_err();
    assert!(matches!(err_eof, Error::SqlParseError { .. }));

    let err_literal = treedb::sql::parse_statement("EXPLAIN 123;").unwrap_err();
    assert!(matches!(err_literal, Error::SqlParseError { .. }));
}

// ============================================================================
// 2. PhysicalPlan Formatting & Access Path Inspections
// ============================================================================

#[test]
fn test_explain_scan_access_paths() {
    let ctx = TestContext::new("access_paths");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT, age INT, score DOUBLE);",
    )
    .unwrap();
    txn.execute_statement("CREATE INDEX idx_email ON users (email);")
        .unwrap();

    // 1. Full table scan
    let plan_full = txn
        .explain_sql("SELECT * FROM users WHERE age > 20;")
        .unwrap();
    assert!(plan_full.contains("TableScan (table: users, access: FullTableScan"));

    // 2. Primary Key Point Lookup
    let plan_pk_point = txn
        .explain_sql("SELECT * FROM users WHERE id = 42;")
        .unwrap();
    assert!(plan_pk_point.contains("TableScan (table: users, access: PrimaryPointLookup(key=42)"));

    // 3. Primary Key Range Scan
    let plan_pk_range = txn
        .explain_sql("SELECT * FROM users WHERE id >= 10 AND id < 50;")
        .unwrap();
    assert!(plan_pk_range
        .contains("TableScan (table: users, access: PrimaryRangeScan(start=10, end=50)"));

    // 4. Secondary Index Point Lookup
    let plan_sec_point = txn
        .explain_sql("SELECT * FROM users WHERE email = 'alice@corp.com';")
        .unwrap();
    assert!(
        plan_sec_point.contains("SecondaryIndexScan(idx_email, PointLookup(\"alice@corp.com\"))")
    );

    // 5. Secondary Index Range Scan
    let plan_sec_range = txn
        .explain_sql("SELECT * FROM users WHERE email >= 'a' AND email < 'm';")
        .unwrap();
    assert!(
        plan_sec_range.contains("SecondaryIndexScan(idx_email, RangeScan(start=\"a\", end=\"m\"))")
    );
}

#[test]
fn test_explain_filter_projection_sort_limit_offset() {
    let ctx = TestContext::new("query_clauses");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT, price INT);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT id, name AS item_name FROM items WHERE price > 100 ORDER BY price DESC, id ASC LIMIT 10 OFFSET 5;")
        .unwrap();

    assert!(
        plan.contains("TopN (limit: Some(10), offset: Some(5)) [price Descending, id Ascending]")
    );
    assert!(plan.contains("Projection [id AS id, name AS item_name]"));
    assert!(
        plan.contains("TableScan (table: items, access: FullTableScan, filter: [(price > 100)])")
    );

    let plan_no_limit = txn
        .explain_sql("SELECT id, name AS item_name FROM items WHERE price > 100 ORDER BY price DESC, id ASC;")
        .unwrap();
    assert!(plan_no_limit.contains("Sort [price Descending, id Ascending]"));
}

#[test]
fn test_explain_joins() {
    let ctx = TestContext::new("joins");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, total INT);",
    )
    .unwrap();

    // Inner Join
    let plan_inner = txn
        .explain_sql("SELECT u.name, o.total FROM users u INNER JOIN orders o ON u.id = o.user_id WHERE o.total > 50;")
        .unwrap();
    assert!(
        plan_inner.contains("Join (Inner, on: [(u.id = o.user_id)])")
            || plan_inner.contains("(Inner, on: [(u.id = o.user_id)])")
    );
    assert!(plan_inner.contains("Left: TableScan (table: users AS u, access: FullTableScan)"));
    assert!(plan_inner.contains("Right: TableScan (table: orders AS o, access: FullTableScan"));

    // Inner Join with cross-table filter
    let plan_cross = txn
        .explain_sql("SELECT u.name, o.total FROM users u INNER JOIN orders o ON u.id = o.user_id WHERE u.id > o.total;")
        .unwrap();
    assert!(plan_cross.contains("Filter [(u.id > o.total)]"));

    // Left Join
    let plan_left = txn
        .explain_sql("SELECT u.name, o.total FROM users u LEFT JOIN orders o ON u.id = o.user_id;")
        .unwrap();
    assert!(
        plan_left.contains("Join (Left, on: [(u.id = o.user_id)])")
            || plan_left.contains("(Left, on: [(u.id = o.user_id)])")
    );
}

#[test]
fn test_explain_aggregation_and_having() {
    let ctx = TestContext::new("aggregation");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE employees (id BIGINT PRIMARY KEY, dept TEXT, salary INT);")
        .unwrap();

    let plan = txn
        .explain_sql(
            "SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept HAVING COUNT(*) > 5;",
        )
        .unwrap();

    assert!(plan.contains("Aggregate (group_by: [dept], aggs: [COUNT(*), AVG(salary)])"));
    assert!(plan.contains("Having [(COUNT(*) > 5)]"));
}

#[test]
fn test_explain_window_functions() {
    let ctx = TestContext::new("windows");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE sales (id BIGINT PRIMARY KEY, region TEXT, amount INT);")
        .unwrap();

    let plan = txn
        .explain_sql(
            "SELECT id, region, amount, \
             ROW_NUMBER() OVER (PARTITION BY region ORDER BY amount DESC), \
             RANK() OVER (PARTITION BY region ORDER BY amount ASC) \
             FROM sales;",
        )
        .unwrap();

    assert!(plan.contains("Window"));
    assert!(plan.contains("ROW_NUMBER()"));
    assert!(plan.contains("RANK()"));
    assert!(plan.contains("Partition By:"));
    assert!(plan.contains("region"));
    assert!(plan.contains("Order By:"));
    assert!(plan.contains("amount Descending"));
    assert!(plan.contains("Frame:"));
    assert!(plan.contains("UNBOUNDED PRECEDING -> CURRENT ROW"));
}

#[test]
fn test_explain_subqueries() {
    let ctx = TestContext::new("subqueries");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, total INT);",
    )
    .unwrap();

    // 1. Correlated scalar subquery in WHERE
    let plan_corr = txn
        .explain_sql("SELECT u.name FROM users u WHERE u.id = (SELECT o.user_id FROM orders o WHERE o.user_id = u.id);")
        .unwrap();
    assert!(plan_corr.contains("Correlated Subquery"));
    assert!(plan_corr.contains("Outer references: [u.id]"));

    // 2. IN subquery (decorrelated to Semi Join)
    let plan_in = txn
        .explain_sql("SELECT * FROM users WHERE id IN (SELECT user_id FROM orders);")
        .unwrap();
    assert!(plan_in.contains("Semi"));

    // 3. NOT IN subquery
    let plan_not_in = txn
        .explain_sql("SELECT * FROM users WHERE id NOT IN (SELECT user_id FROM orders);")
        .unwrap();
    assert!(plan_not_in.contains("Kind: NOT IN"));

    // 4. EXISTS subquery (decorrelated to Semi Join)
    let plan_exists = txn
        .explain_sql(
            "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id);",
        )
        .unwrap();
    assert!(plan_exists.contains("Semi"));

    // 5. Non-decorrelatable EXISTS subquery (retains Subquery node)
    let plan_exists_group = txn
        .explain_sql(
            "SELECT * FROM users u WHERE EXISTS (SELECT o.user_id FROM orders o WHERE o.user_id = u.id GROUP BY o.user_id HAVING count(*) > 1);",
        )
        .unwrap();
    assert!(plan_exists_group.contains("Kind: EXISTS"));
}

#[test]
fn test_explain_ctes() {
    let ctx = TestContext::new("ctes");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age INT);")
        .unwrap();

    let plan = txn
        .explain_sql(
            "WITH c1 AS (SELECT id, name FROM users WHERE age > 21), \
             c2 AS (SELECT id FROM c1) \
             SELECT * FROM c2;",
        )
        .unwrap();

    assert!(plan.contains("CTE c1"));
    assert!(plan.contains("CTE c2"));
    assert!(plan.contains("depends on: [c1]"));
    assert!(plan.contains("Main Query"));
}

#[test]
fn test_explain_recursive_cte() {
    let ctx = TestContext::new("recursive_cte");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE seeds (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();

    let plan = txn
        .explain_sql(
            "WITH RECURSIVE nums AS (\
                 SELECT val AS n FROM seeds WHERE id = 1 \
                 UNION ALL \
                 SELECT n + 1 FROM nums WHERE n < 100\
             ) SELECT * FROM nums;",
        )
        .unwrap();

    assert!(plan.contains("Recursive CTE nums"));
    assert!(plan.contains("Anchor"));
    assert!(plan.contains("Recursive Member"));
    assert!(plan.contains("Max Iterations: 1000"));
    assert!(plan.contains("Max Rows: 1000000"));
    assert!(plan.contains("Main Query"));
}

#[test]
fn test_explain_all_set_operations() {
    let ctx = TestContext::new("set_operations");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();

    let ops = [
        ("UNION", "UNION"),
        ("UNION ALL", "UNION ALL"),
        ("INTERSECT", "INTERSECT"),
        ("INTERSECT ALL", "INTERSECT ALL"),
        ("EXCEPT", "EXCEPT"),
        ("EXCEPT ALL", "EXCEPT ALL"),
    ];

    for (op_sql, op_name) in ops {
        let sql = format!("SELECT val FROM t1 {op_sql} SELECT val FROM t2;");
        let plan = txn.explain_sql(&sql).unwrap();
        assert!(
            plan.contains(&format!("Set Operation: {op_name}")),
            "Expected plan to contain Set Operation: {op_name}, found:\n{plan}"
        );
        assert!(plan.contains("Left"));
        assert!(plan.contains("Right"));
    }

    // Nested / parenthesized set operations
    let nested_sql =
        "(SELECT val FROM t1 UNION SELECT val FROM t2) INTERSECT (SELECT val FROM t1);";
    let nested_plan = txn.explain_sql(nested_sql).unwrap();
    assert!(nested_plan.contains("Set Operation: INTERSECT"));
    assert!(nested_plan.contains("Set Operation: UNION"));
}

// ============================================================================
// 3. Optimizer Lifecycle and Transaction Visibility
// ============================================================================

#[test]
fn test_explain_optimizer_index_lifecycle() {
    let ctx = TestContext::new("index_lifecycle");
    let mut db = ctx.db();

    // 1. Initial table without secondary index
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT);")
            .unwrap();
        txn.commit().unwrap();
    }

    // 2. EXPLAIN shows FullTableScan
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn
            .explain_sql("SELECT * FROM users WHERE email = 'test@example.com';")
            .unwrap();
        assert!(plan.contains("FullTableScan"));
        assert!(!plan.contains("SecondaryIndexScan"));
    }

    // 3. CREATE INDEX
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE INDEX idx_users_email ON users (email);")
            .unwrap();
        txn.commit().unwrap();
    }

    // 4. EXPLAIN shows SecondaryIndexScan
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn
            .explain_sql("SELECT * FROM users WHERE email = 'test@example.com';")
            .unwrap();
        assert!(plan.contains("SecondaryIndexScan(idx_users_email"));
        assert!(!plan.contains("FullTableScan"));
    }

    // 5. DROP INDEX
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("DROP INDEX idx_users_email;")
            .unwrap();
        txn.commit().unwrap();
    }

    // 6. EXPLAIN reverts to FullTableScan
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn
            .explain_sql("SELECT * FROM users WHERE email = 'test@example.com';")
            .unwrap();
        assert!(plan.contains("FullTableScan"));
        assert!(!plan.contains("SecondaryIndexScan"));
    }
}

#[test]
fn test_explain_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_visibility");
    let mut db = ctx.db();

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE accounts (id BIGINT PRIMARY KEY, code TEXT);")
            .unwrap();
        txn.commit().unwrap();
    }

    // In an active transaction, create an index and inspect via EXPLAIN
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE INDEX idx_code ON accounts (code);")
            .unwrap();

        // Must see index in EXPLAIN before commit
        let plan = txn
            .explain_sql("SELECT * FROM accounts WHERE code = 'A1';")
            .unwrap();
        assert!(plan.contains("SecondaryIndexScan(idx_code"));

        // Rollback transaction
        txn.abort().unwrap();
    }

    // New transaction after rollback: index is gone, EXPLAIN reverts to full scan
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn
            .explain_sql("SELECT * FROM accounts WHERE code = 'A1';")
            .unwrap();
        assert!(plan.contains("FullTableScan"));
        assert!(!plan.contains("SecondaryIndexScan"));
    }
}

// ============================================================================
// 4. QueryResult Output & Invariant Guarantees (No Execution, No Mutation)
// ============================================================================

#[test]
fn test_explain_query_result_format() {
    let ctx = TestContext::new("result_format");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();

    // Execute EXPLAIN via execute_sql (which calls execute_statement)
    let res = txn
        .execute_sql("EXPLAIN SELECT id, name FROM users WHERE id = 5;")
        .unwrap();

    // Result must have exactly 1 column named "plan" of type DataType::String
    assert_eq!(res.schema().columns.len(), 1);
    assert_eq!(res.schema().columns[0].name, "plan");
    assert_eq!(res.schema().columns[0].data_type, DataType::String);

    // Exactly 1 row containing the textual plan
    assert_eq!(res.rows().len(), 1);
    match &res.rows()[0].values[0] {
        Value::String(plan_str) => {
            assert!(plan_str.contains("PrimaryPointLookup(key=5)"));
            assert!(plan_str.contains("Projection [id AS id, name AS name]"));
        }
        other => panic!("expected String value in plan column, found {other:?}"),
    }
}

#[test]
fn test_explain_no_execution_guarantee() {
    let ctx = TestContext::new("no_execution");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();

    // Running EXPLAIN must not perform scans or modify data
    let res = txn.execute_sql("EXPLAIN SELECT * FROM users;").unwrap();
    assert_eq!(res.rows().len(), 1);

    // Data remains unchanged
    let query_res = txn.execute_sql("SELECT COUNT(*) FROM users;").unwrap();
    assert_eq!(query_res.rows()[0].values[0], Value::UInt64(2));
}

#[test]
fn test_explain_no_persistence_mutation() {
    let ctx = TestContext::new("no_mutation");
    let mut db = ctx.db();

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, sku TEXT);")
            .unwrap();
        txn.execute_statement("INSERT INTO items VALUES (1, 'SKU1');")
            .unwrap();
        txn.commit().unwrap();
    }

    let initial_db_size = std::fs::metadata(&ctx.db_path).unwrap().len();

    // Run 10 different EXPLAIN queries inside an active transaction
    {
        let mut txn = db.begin_transaction().unwrap();
        let initial_wal_size = std::fs::metadata(&ctx.wal_path).unwrap().len();

        for _ in 0..10 {
            let _ = txn
                .execute_sql("EXPLAIN SELECT * FROM items WHERE id = 1;")
                .unwrap();
            let _ = txn
                .execute_sql("EXPLAIN SELECT sku FROM items WHERE sku = 'SKU1';")
                .unwrap();
            let _ = txn.explain_sql("SELECT COUNT(*) FROM items;").unwrap();
        }

        let post_wal_size = std::fs::metadata(&ctx.wal_path).unwrap().len();
        assert_eq!(
            initial_wal_size, post_wal_size,
            "WAL file size changed during EXPLAIN execution!"
        );
    }

    let post_db_size = std::fs::metadata(&ctx.db_path).unwrap().len();
    assert_eq!(
        initial_db_size, post_db_size,
        "DB file size changed during EXPLAIN!"
    );
}

#[test]
fn test_explain_determinism() {
    let ctx = TestContext::new("determinism");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT, total INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_b_a_id ON b (a_id);")
        .unwrap();

    let sql = "EXPLAIN WITH cte AS (SELECT a.id, b.total FROM a INNER JOIN b ON a.id = b.a_id WHERE b.total > 100) \
               SELECT id, total, ROW_NUMBER() OVER (PARTITION BY id ORDER BY total DESC) FROM cte;";

    let plan1 = txn.explain_sql(sql).unwrap();
    let plan2 = txn.explain_sql(sql).unwrap();
    let plan3 = txn.explain_sql(sql).unwrap();

    assert_eq!(plan1, plan2);
    assert_eq!(plan2, plan3);
}

#[test]
fn test_explain_binder_error_propagation() {
    let ctx = TestContext::new("binder_errors");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();

    // 1. Unknown table
    let err_tbl = txn.explain_sql("SELECT * FROM nonexistent;").unwrap_err();
    assert!(matches!(err_tbl, Error::TableNotFound(_)));

    // 2. Unknown column
    let err_col = txn
        .explain_sql("SELECT nonexistent FROM users;")
        .unwrap_err();
    assert!(matches!(err_col, Error::ColumnNotFound(_)));

    // 3. Ungrouped column error
    let err_grp = txn
        .explain_sql("SELECT id, COUNT(*) FROM users;")
        .unwrap_err();
    assert!(matches!(err_grp, Error::UngroupedColumn(_)));
}
