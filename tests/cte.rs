use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

fn col_names(res: &treedb::query::QueryResult) -> Vec<&str> {
    res.schema.columns.iter().map(|c| c.name.as_str()).collect()
}

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_cte_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_cte_{name}.treedb.wal"));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
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
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

fn setup_sample_db(db: &mut TreeDb) {
    let mut txn = db.begin().unwrap();

    let users_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, true),
            Column::new("balance", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("users", users_schema).unwrap();

    let orders_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("user_id", DataType::UInt64, true),
            Column::new("amount", DataType::Float64, false),
            Column::new("status", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("orders", orders_schema).unwrap();

    txn.create_index("orders", "idx_orders_user_id", "user_id", false)
        .unwrap();

    // Insert users:
    // 1: Alice, age 30, balance 100.0
    // 2: Bob, age 20, balance 50.0
    // 3: Charlie, age 40, balance NULL
    // 4: David, age NULL, balance 200.0
    txn.execute_statement(
        "INSERT INTO users (id, name, age, balance) VALUES (1, 'Alice', 30, 100.0)",
    )
    .unwrap();
    txn.execute_statement("INSERT INTO users (id, name, age, balance) VALUES (2, 'Bob', 20, 50.0)")
        .unwrap();
    txn.execute_statement(
        "INSERT INTO users (id, name, age, balance) VALUES (3, 'Charlie', 40, NULL)",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO users (id, name, age, balance) VALUES (4, 'David', NULL, 200.0)",
    )
    .unwrap();

    // Insert orders:
    // 10: user 1, amount 25.0, 'completed'
    // 20: user 1, amount 75.0, 'completed'
    // 30: user 2, amount 150.0, 'pending'
    // 40: user NULL, amount 10.0, 'guest'
    txn.execute_statement(
        "INSERT INTO orders (id, user_id, amount, status) VALUES (10, 1, 25.0, 'completed')",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO orders (id, user_id, amount, status) VALUES (20, 1, 75.0, 'completed')",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO orders (id, user_id, amount, status) VALUES (30, 2, 150.0, 'pending')",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO orders (id, user_id, amount, status) VALUES (40, NULL, 10.0, 'guest')",
    )
    .unwrap();

    txn.commit().unwrap();
}

#[test]
fn test_cte_single_basic() {
    let ctx = TestContext::new("single_basic");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH active_users AS (
        SELECT id, name, age
        FROM users
        WHERE age >= 30
    )
    SELECT id, name
    FROM active_users
    ORDER BY id ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(col_names(&res), &["id", "name"]);
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::UInt64(3));
    assert_eq!(res.rows[1].values[1], Value::String("Charlie".into()));
}

#[test]
fn test_cte_multiple_independent() {
    let ctx = TestContext::new("multi_indep");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH
        u AS (
            SELECT id, name FROM users WHERE id <= 2
        ),
        o AS (
            SELECT id AS order_id, user_id, amount FROM orders WHERE amount > 50.0
        )
    SELECT u.name, o.order_id, o.amount
    FROM u
    JOIN o ON u.id = o.user_id
    ORDER BY o.order_id ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(col_names(&res), &["name", "order_id", "amount"]);
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(20));
    assert_eq!(res.rows[0].values[2], Value::Float64(75.0));

    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[1].values[1], Value::UInt64(30));
    assert_eq!(res.rows[1].values[2], Value::Float64(150.0));
}

#[test]
fn test_cte_chain_dependency() {
    let ctx = TestContext::new("chain_dep");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH
        a AS (
            SELECT id, name, age, balance FROM users
        ),
        b AS (
            SELECT id, name, balance FROM a WHERE age > 25
        ),
        c AS (
            SELECT id, name FROM b WHERE balance IS NOT NULL
        )
    SELECT id, name FROM c;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
}

#[test]
fn test_cte_explicit_column_list() {
    let ctx = TestContext::new("explicit_cols");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH users_view(uid, uname) AS (
        SELECT id, name FROM users WHERE id = 1
    )
    SELECT uid, uname FROM users_view;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(col_names(&res), &["uid", "uname"]);
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
}

#[test]
fn test_cte_column_count_mismatch() {
    let ctx = TestContext::new("col_count_mismatch");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH users_view(uid, uname, extra) AS (
        SELECT id, name FROM users
    )
    SELECT * FROM users_view;";

    let res = txn.execute_sql(sql);
    match res {
        Err(Error::CteColumnCountMismatch {
            cte,
            expected,
            found,
        }) => {
            assert_eq!(cte, "users_view");
            assert_eq!(expected, 3);
            assert_eq!(found, 2);
        }
        other => panic!("expected CteColumnCountMismatch, got {other:?}"),
    }
}

#[test]
fn test_cte_duplicate_column_name() {
    let ctx = TestContext::new("dup_col_name");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH users_view(uid, uid) AS (
        SELECT id, name FROM users
    )
    SELECT * FROM users_view;";

    let res = txn.execute_sql(sql);
    match res {
        Err(Error::DuplicateCteColumnName { cte, column }) => {
            assert_eq!(cte, "users_view");
            assert_eq!(column, "uid");
        }
        other => panic!("expected DuplicateCteColumnName, got {other:?}"),
    }
}

#[test]
fn test_cte_duplicate_cte_name() {
    let ctx = TestContext::new("dup_cte_name");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH
        a AS (SELECT id FROM users),
        a AS (SELECT name FROM users)
    SELECT * FROM a;";

    let res = txn.execute_sql(sql);
    match res {
        Err(Error::DuplicateCteName(name)) => {
            assert_eq!(name, "a");
        }
        other => panic!("expected DuplicateCteName, got {other:?}"),
    }
}

#[test]
fn test_cte_forward_reference_error() {
    let ctx = TestContext::new("forward_ref");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH
        a AS (SELECT * FROM b),
        b AS (SELECT * FROM users)
    SELECT * FROM a;";

    let res = txn.execute_sql(sql);
    assert!(
        res.is_err(),
        "forward reference from CTE 'a' to 'b' should fail"
    );
}

#[test]
fn test_cte_self_reference_error() {
    let ctx = TestContext::new("self_ref");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH a AS (SELECT * FROM a) SELECT * FROM a;";

    let res = txn.execute_sql(sql);
    assert!(res.is_err(), "self-referencing CTE 'a' should fail");
}

#[test]
fn test_cte_qualified_references_and_aliases() {
    let ctx = TestContext::new("qual_aliases");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    // 1. Qualified CTE column reference (active.id)
    let sql1 = "WITH active AS (
        SELECT id, name FROM users WHERE id = 1
    )
    SELECT active.id, active.name FROM active;";
    let res1 = txn.execute_sql(sql1).unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[0], Value::UInt64(1));

    // 2. CTE table alias (a.id)
    let sql2 = "WITH active AS (
        SELECT id, name FROM users WHERE id = 1
    )
    SELECT a.id, a.name FROM active a;";
    let res2 = txn.execute_sql(sql2).unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::UInt64(1));
}

#[test]
fn test_cte_recursive_rejection() {
    let ctx = TestContext::new("recursive_rejection");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE a AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM a WHERE n < 5) SELECT * FROM a;";
    let res = txn.execute_sql(sql);
    assert!(res.is_err(), "WITH RECURSIVE syntax must be rejected");
}

#[test]
fn test_cte_shadowing_physical_table() {
    let ctx = TestContext::new("shadowing");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    // CTE 'users' selects only id = 1 from base table 'users'
    let sql = "WITH users AS (
        SELECT id, name FROM users WHERE id = 1
    )
    SELECT id, name FROM users;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
}

#[test]
fn test_cte_with_left_and_inner_joins() {
    let ctx = TestContext::new("cte_joins");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH
        u AS (
            SELECT id, name FROM users WHERE id = 1 OR id = 3
        ),
        o AS (
            SELECT id AS order_id, user_id, amount FROM orders
        )
    SELECT u.name, o.order_id
    FROM u
    LEFT JOIN o ON u.id = o.user_id
    ORDER BY u.name ASC, o.order_id ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(col_names(&res), &["name", "order_id"]);
    assert_eq!(res.rows.len(), 3);
    // Alice has order 10 and 20
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(10));
    assert_eq!(res.rows[1].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[1], Value::UInt64(20));
    // Charlie has NULL order
    assert_eq!(res.rows[2].values[0], Value::String("Charlie".into()));
    assert_eq!(res.rows[2].values[1], Value::Null);
}

#[test]
fn test_cte_with_aggregation_and_having() {
    let ctx = TestContext::new("cte_agg");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH order_stats AS (
        SELECT user_id, COUNT(*) AS order_cnt, SUM(amount) AS total_spent
        FROM orders
        WHERE user_id IS NOT NULL
        GROUP BY user_id
        HAVING COUNT(*) > 1
    )
    SELECT u.name, s.order_cnt, s.total_spent
    FROM order_stats s
    JOIN users u ON u.id = s.user_id;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(res.rows[0].values[2], Value::Float64(100.0));
}

#[test]
fn test_cte_subqueries_integration() {
    let ctx = TestContext::new("cte_subqueries");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();

    // 1. CTE referenced inside IN subquery
    let sql1 = "WITH high_spenders AS (
        SELECT user_id FROM orders WHERE amount >= 75.0 AND user_id IS NOT NULL
    )
    SELECT name
    FROM users
    WHERE id IN (SELECT user_id FROM high_spenders)
    ORDER BY name ASC;";

    let res1 = txn.execute_sql(sql1).unwrap();
    assert_eq!(res1.rows.len(), 2);
    assert_eq!(res1.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res1.rows[1].values[0], Value::String("Bob".into()));

    // 2. CTE referenced inside correlated EXISTS subquery
    let sql2 = "WITH active_orders AS (
        SELECT id, user_id, amount FROM orders WHERE status = 'completed'
    )
    SELECT u.name
    FROM users u
    WHERE EXISTS (
        SELECT 1 FROM active_orders ao WHERE ao.user_id = u.id
    );";

    let res2 = txn.execute_sql(sql2).unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("Alice".into()));

    // 3. Subquery inside CTE body
    let sql3 = "WITH users_with_orders AS (
        SELECT u.id, u.name
        FROM users u
        WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)
    )
    SELECT name FROM users_with_orders ORDER BY name ASC;";

    let res3 = txn.execute_sql(sql3).unwrap();
    assert_eq!(res3.rows.len(), 2);
    assert_eq!(res3.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res3.rows[1].values[0], Value::String("Bob".into()));
}

#[test]
fn test_cte_multiple_references_same_cte() {
    let ctx = TestContext::new("multi_ref_same_cte");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH u AS (
        SELECT id, name, age FROM users WHERE age IS NOT NULL
    )
    SELECT a.name AS name1, b.name AS name2
    FROM u a
    JOIN u b ON a.age < b.age
    ORDER BY a.name ASC, b.name ASC;";

    let res = txn.execute_sql(sql).unwrap();
    // Bob (20) < Alice (30)
    // Bob (20) < Charlie (40)
    // Alice (30) < Charlie (40)
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::String("Charlie".into()));

    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[1].values[1], Value::String("Alice".into()));

    assert_eq!(res.rows[2].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[2].values[1], Value::String("Charlie".into()));
}

#[test]
fn test_cte_order_by_limit_inside_and_outside() {
    let ctx = TestContext::new("order_limit");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH top_users AS (
        SELECT id, name, balance
        FROM users
        WHERE balance IS NOT NULL
        ORDER BY balance DESC
        LIMIT 2
    )
    SELECT id, name, balance
    FROM top_users
    ORDER BY balance ASC
    LIMIT 1;";

    let inner_plan = txn.plan_sql("SELECT id, name, balance FROM users WHERE balance IS NOT NULL ORDER BY balance DESC LIMIT 2;").unwrap();
    eprintln!("Inner plan:\n{inner_plan:?}");
    let explain = txn.explain_sql(sql).unwrap();
    eprintln!("Explain:\n{explain}");
    let res = txn.execute_sql(sql).unwrap();
    eprintln!("Res: {:?}", res.rows);
    assert_eq!(res.rows.len(), 1);
    // top 2 by balance DESC: David (200.0), Alice (100.0)
    // ordered by balance ASC LIMIT 1: Alice (100.0)
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[2], Value::Float64(100.0));
}

#[test]
fn test_cte_optimizer_and_explain() {
    let ctx = TestContext::new("optimizer_explain");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "WITH active_orders AS (
        SELECT id, user_id, amount
        FROM orders
        WHERE user_id = 1
    )
    SELECT *
    FROM active_orders;";

    let explain = txn.explain_sql(sql).unwrap();
    assert!(explain.contains("CTE active_orders"));
    assert!(explain.contains("Main Query"));
    // orders has secondary index on user_id, so it should use index lookup in CTE
    assert!(explain.contains("SecondaryIndexScan"));
}

#[test]
fn test_cte_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_visibility");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let mut txn = db.begin().unwrap();
    txn.execute_statement(
        "INSERT INTO users (id, name, age, balance) VALUES (5, 'Eve', 28, 500.0)",
    )
    .unwrap();

    let sql = "WITH u AS (
        SELECT * FROM users WHERE age < 30
    )
    SELECT name FROM u ORDER BY name ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Eve".into()));

    txn.abort().unwrap();

    // After abort, Eve is not visible
    let mut txn2 = db.begin().unwrap();
    let res2 = txn2.execute_sql(sql).unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("Bob".into()));
}

#[test]
fn test_cte_non_persistence_and_reopen() {
    let ctx = TestContext::new("non_persistence");
    let mut db = ctx.db();
    setup_sample_db(&mut db);

    let page_count_before = {
        let mut txn = db.begin().unwrap();
        let pc = txn.buffer_pool_mut().page_manager().header().page_count;
        let _ = txn
            .execute_sql(
                "WITH u AS (SELECT * FROM users), o AS (SELECT * FROM orders) SELECT * FROM u JOIN o ON u.id = o.user_id;",
            )
            .unwrap();
        pc
    };

    let page_count_after = {
        let mut txn = db.begin().unwrap();
        txn.buffer_pool_mut().page_manager().header().page_count
    };

    assert_eq!(
        page_count_before, page_count_after,
        "CTE execution must not allocate persistent pages"
    );

    // Reopen database and verify results remain identical
    drop(db);
    let mut reopened_db = ctx.db();
    let mut txn = reopened_db.begin().unwrap();
    let res = txn
        .execute_sql("WITH u AS (SELECT id, name FROM users WHERE id = 1) SELECT * FROM u;")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
}

// ============================================================================
// Randomized Reference Oracle Test
// ============================================================================

#[derive(Clone, Debug)]
struct RefRow {
    id: u64,
    val: i64,
    cat: String,
    score: Option<f64>,
}

#[test]
fn test_cte_randomized_reference_oracle() {
    let ctx = TestContext::new("randomized_oracle");
    let mut db = ctx.db();

    // 1. Create table and insert randomized dataset
    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, false),
            Column::new("cat", DataType::String, false),
            Column::new("score", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("items", schema).unwrap();

    let mut reference_data = Vec::new();
    let mut lcg_state: u64 = 42;
    let mut lcg = || {
        lcg_state = lcg_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        lcg_state
    };

    let cats = ["A", "B", "C"];
    for i in 1..=50u64 {
        let val = (lcg() % 100) as i64;
        let cat = cats[(lcg() % 3) as usize].to_string();
        let score = if (lcg() % 5) == 0 {
            None
        } else {
            Some(((lcg() % 1000) as f64) / 10.0)
        };

        let row = RefRow {
            id: i,
            val,
            cat: cat.clone(),
            score,
        };
        reference_data.push(row);

        let score_sql = match score {
            Some(s) => format!("{s:.1}"),
            None => "NULL".to_string(),
        };
        let ins = format!(
            "INSERT INTO items (id, val, cat, score) VALUES ({i}, {val}, '{cat}', {score_sql});"
        );
        txn.execute_statement(&ins).unwrap();
    }
    txn.commit().unwrap();

    // 2. Run test queries against TreeDB and independent in-memory reference evaluator
    let mut txn = db.begin().unwrap();

    // Query 1: Filter in CTE, projection in main
    {
        let threshold = 50i64;
        let sql = format!(
            "WITH filtered AS (
                SELECT id, val, cat, score FROM items WHERE val >= {threshold}
            )
            SELECT id, val, cat FROM filtered ORDER BY id ASC;"
        );
        let res = txn.execute_sql(&sql).unwrap();

        let mut expected: Vec<&RefRow> = reference_data
            .iter()
            .filter(|r| r.val >= threshold)
            .collect();
        expected.sort_by_key(|r| r.id);

        assert_eq!(res.rows.len(), expected.len());
        for (actual_row, exp) in res.rows.iter().zip(expected.iter()) {
            assert_eq!(actual_row.values[0], Value::UInt64(exp.id));
            assert_eq!(actual_row.values[1], Value::Int64(exp.val));
            assert_eq!(actual_row.values[2], Value::String(exp.cat.clone()));
        }
    }

    // Query 2: Chained CTE with column renaming and null filtering
    {
        let sql = "WITH
            step1(item_id, item_val, item_cat, item_score) AS (
                SELECT id, val, cat, score FROM items WHERE score IS NOT NULL
            ),
            step2 AS (
                SELECT item_id, item_val FROM step1 WHERE item_cat = 'A'
            )
            SELECT item_id, item_val FROM step2 ORDER BY item_id ASC;";
        let res = txn.execute_sql(sql).unwrap();

        let mut expected: Vec<&RefRow> = reference_data
            .iter()
            .filter(|r| r.score.is_some() && r.cat == "A")
            .collect();
        expected.sort_by_key(|r| r.id);

        assert_eq!(res.rows.len(), expected.len());
        for (actual_row, exp) in res.rows.iter().zip(expected.iter()) {
            assert_eq!(actual_row.values[0], Value::UInt64(exp.id));
            assert_eq!(actual_row.values[1], Value::Int64(exp.val));
        }
    }

    // Query 3: CTE with Aggregation and JOIN
    {
        let sql = "WITH cat_counts AS (
            SELECT cat, COUNT(*) AS cnt FROM items GROUP BY cat
        )
        SELECT cat, cnt FROM cat_counts ORDER BY cat ASC;";
        let res = txn.execute_sql(sql).unwrap();

        let mut counts: HashMap<String, i64> = HashMap::new();
        for r in &reference_data {
            *counts.entry(r.cat.clone()).or_insert(0) += 1;
        }
        let mut expected: Vec<(String, i64)> = counts.into_iter().collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(res.rows.len(), expected.len());
        for (actual_row, (cat, count)) in res.rows.iter().zip(expected.iter()) {
            assert_eq!(actual_row.values[0], Value::String(cat.clone()));
            assert_eq!(actual_row.values[1], Value::UInt64(*count as u64));
        }
    }
}
