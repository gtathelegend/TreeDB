use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_subquery_{name}.treedb"));
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

fn setup_ecommerce_db(db: &mut TreeDb) {
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

    // Create secondary index on orders.user_id
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

fn query_txn(
    txn: &mut treedb::engine::transaction::Transaction<'_>,
    sql: &str,
) -> Result<treedb::query::QueryResult, Error> {
    txn.execute_statement(sql).and_then(|res| res.into_query())
}

#[test]
fn test_scalar_subquery_single_row_match() {
    let ctx = TestContext::new("scalar_single_row");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id = (SELECT user_id FROM orders WHERE id = 30)",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Bob".into()));
}

#[test]
fn test_scalar_subquery_zero_rows_is_null() {
    let ctx = TestContext::new("scalar_zero_rows");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Subquery returns 0 rows -> scalar evaluates to NULL -> id = NULL is false for all rows
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id = (SELECT user_id FROM orders WHERE id = 999)",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 0);

    // Using IS NULL on scalar subquery yielding 0 rows
    let res2 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE (SELECT user_id FROM orders WHERE id = 999) IS NULL ORDER BY id",
    )
    .unwrap();
    assert_eq!(res2.rows.len(), 4);
}

#[test]
fn test_scalar_subquery_multiple_rows_error() {
    let ctx = TestContext::new("scalar_multi_rows_err");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Subquery returns 2 rows (orders for user 1) -> ScalarSubqueryMultipleRows
    let err = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id = (SELECT user_id FROM orders WHERE user_id = 1)",
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::ScalarSubqueryMultipleRows),
        "expected ScalarSubqueryMultipleRows, got {err:?}"
    );
}

#[test]
fn test_scalar_subquery_column_count_mismatch() {
    let ctx = TestContext::new("scalar_col_count_err");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Subquery projects 2 columns -> SubqueryColumnCountMismatch
    let err = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id = (SELECT id, user_id FROM orders WHERE id = 10)",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SubqueryColumnCountMismatch {
                expected: 1,
                found: 2
            }
        ),
        "expected SubqueryColumnCountMismatch, got {err:?}"
    );
}

#[test]
fn test_in_subquery_basic() {
    let ctx = TestContext::new("in_subquery_basic");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount > 50.0) ORDER BY id",
    )
    .unwrap();
    // user 1 has order 20 (75.0), user 2 has order 30 (150.0)
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
}

#[test]
fn test_in_subquery_empty() {
    let ctx = TestContext::new("in_subquery_empty");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount > 1000.0)",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 0);
}

#[test]
fn test_in_subquery_column_count_mismatch() {
    let ctx = TestContext::new("in_subquery_col_mismatch");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    let err = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id IN (SELECT id, user_id FROM orders)",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SubqueryColumnCountMismatch {
                expected: 1,
                found: 2
            }
        ),
        "expected SubqueryColumnCountMismatch, got {err:?}"
    );
}

#[test]
fn test_in_subquery_with_nulls_3vl() {
    let ctx = TestContext::new("in_subquery_nulls_3vl");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // orders.user_id contains [1, 1, 2, NULL]
    // id IN (1, 1, 2, NULL):
    // id=1 -> TRUE
    // id=2 -> TRUE
    // id=3 -> NULL (since 3 is not in {1,2} but set contains NULL) -> treated as false in WHERE
    // id=4 -> NULL (since 4 is not in {1,2} but set contains NULL) -> treated as false in WHERE
    let res = query_txn(
        &mut txn,
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[1].values[0], Value::UInt64(2));
}

#[test]
fn test_not_in_subquery_with_nulls_3vl() {
    let ctx = TestContext::new("not_in_subquery_nulls_3vl");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // orders.user_id contains [1, 1, 2, NULL]
    // id NOT IN (1, 1, 2, NULL):
    // id=1 -> FALSE
    // id=2 -> FALSE
    // id=3 -> NULL (since 3 != NULL is UNKNOWN, NOT IN is UNKNOWN/NULL) -> excluded from WHERE!
    // id=4 -> NULL -> excluded from WHERE!
    // Result should be EMPTY because of the NULL in subquery!
    let res = query_txn(
        &mut txn,
        "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders)",
    )
    .unwrap();
    assert_eq!(
        res.rows.len(),
        0,
        "NOT IN with NULL in subquery must return 0 rows under 3VL"
    );

    // If subquery filters out NULL (WHERE user_id IS NOT NULL), then id NOT IN (1, 2)
    // id=3 -> TRUE
    // id=4 -> TRUE
    let res2 = query_txn(
        &mut txn,
        "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE user_id IS NOT NULL) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res2.rows.len(), 2);
    assert_eq!(res2.rows[0].values[0], Value::UInt64(3));
    assert_eq!(res2.rows[1].values[0], Value::UInt64(4));
}

#[test]
fn test_not_in_subquery_empty() {
    let ctx = TestContext::new("not_in_empty");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // NOT IN on empty subquery returns TRUE for non-null outer values
    let res = query_txn(
        &mut txn,
        "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE 1 = 0) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 4);
}

#[test]
fn test_exists_and_not_exists_uncorrelated() {
    let ctx = TestContext::new("exists_uncorrelated");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // EXISTS with matching rows returns true for all rows in users
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE EXISTS (SELECT * FROM orders WHERE amount > 100.0) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 4);

    // EXISTS with 0 matching rows returns false
    let res2 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE EXISTS (SELECT * FROM orders WHERE amount > 1000.0)",
    )
    .unwrap();
    assert_eq!(res2.rows.len(), 0);

    // NOT EXISTS with 0 matching rows returns true
    let res3 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE NOT EXISTS (SELECT * FROM orders WHERE amount > 1000.0) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res3.rows.len(), 4);

    // EXISTS with multiple columns and NULL values in projection
    let res4 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE EXISTS (SELECT NULL, NULL, id FROM orders) ORDER BY id",
    )
    .unwrap();
    assert_eq!(res4.rows.len(), 4);
}

#[test]
fn test_correlated_exists() {
    let ctx = TestContext::new("corr_exists");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Users who have at least one order: Alice (1) and Bob (2)
    let res = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));

    // Users who have no orders: Charlie (3) and David (4)
    let res_not = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id",
    )
    .unwrap();
    assert_eq!(res_not.rows.len(), 2);
    assert_eq!(res_not.rows[0].values[0], Value::String("Charlie".into()));
    assert_eq!(res_not.rows[1].values[0], Value::String("David".into()));
}

#[test]
fn test_correlated_scalar_subquery() {
    let ctx = TestContext::new("corr_scalar");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Users whose balance equals the MAX amount of their orders
    // Alice balance=100.0, max order=75.0 (no match)
    // Bob balance=50.0, max order=150.0 (no match)
    // Let's test with comparison: users whose balance > MIN order amount
    // Alice balance=100.0 > 25.0 -> match
    // Bob balance=50.0 > 150.0 -> no match
    let res = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE u.balance > (SELECT MIN(o.amount) FROM orders o WHERE o.user_id = u.id)",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
}

#[test]
fn test_binding_scope_shadowing() {
    let ctx = TestContext::new("scope_shadowing");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // In inner query, `id` should resolve to `o.id` (innermost), not `u.id`
    // `WHERE id = o.id` is tautologically true for each order row
    let res = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE id = o.id AND o.user_id = u.id) ORDER BY u.id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
}

#[test]
fn test_binding_errors() {
    let ctx = TestContext::new("binding_errors");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Unknown column in subquery
    let err1 = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.non_existent = u.id)",
    )
    .unwrap_err();
    assert!(
        matches!(err1, Error::ColumnNotFound(..)),
        "expected ColumnNotFound, got {err1:?}"
    );

    // Unknown outer table qualifier
    let err2 = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = nonexistent.id)",
    )
    .unwrap_err();
    assert!(
        matches!(
            err2,
            Error::ColumnNotFound(..) | Error::UnknownTableAlias(..)
        ),
        "expected ColumnNotFound or UnknownTableAlias, got {err2:?}"
    );
}

#[test]
fn test_deep_nested_correlation() {
    let ctx = TestContext::new("deep_nested_corr");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();

    // Create third table `payments`: id (PK), order_id (FK to orders)
    let payments_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("order_id", DataType::UInt64, false),
            Column::new("amount", DataType::Float64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("payments", payments_schema).unwrap();

    txn.execute_statement("INSERT INTO payments (id, order_id, amount) VALUES (100, 10, 25.0)")
        .unwrap();
    txn.execute_statement("INSERT INTO payments (id, order_id, amount) VALUES (200, 30, 150.0)")
        .unwrap();

    // Deep nesting: users u -> orders o -> payments p, where p checks p.amount <= u.balance
    // Alice balance=100.0, order 10 payment 25.0 <= 100.0 -> Alice matches
    // Bob balance=50.0, order 30 payment 150.0 <= 50.0 is false -> Bob doesn't match
    let res = query_txn(
        &mut txn,
        "SELECT u.name FROM users u WHERE EXISTS (
            SELECT 1 FROM orders o WHERE o.user_id = u.id AND EXISTS (
                SELECT 1 FROM payments p WHERE p.order_id = o.id AND p.amount <= u.balance
            )
        )",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
}

#[test]
fn test_subquery_with_joins() {
    let ctx = TestContext::new("subquery_with_joins");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();

    // Create user_profiles table
    let profile_schema = Schema::new(
        vec![
            Column::new("user_id", DataType::UInt64, false),
            Column::new("tier", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("profiles", profile_schema).unwrap();
    txn.execute_statement("INSERT INTO profiles (user_id, tier) VALUES (1, 'gold')")
        .unwrap();
    txn.execute_statement("INSERT INTO profiles (user_id, tier) VALUES (2, 'silver')")
        .unwrap();

    // Outer query has JOIN between users and profiles, subquery correlates with users.id and profiles.tier
    let res = query_txn(
        &mut txn,
        "SELECT u.name, p.tier FROM users u
         INNER JOIN profiles p ON p.user_id = u.id
         WHERE EXISTS (
             SELECT 1 FROM orders o
             WHERE o.user_id = u.id AND (p.tier = 'gold' OR o.amount > 100.0)
         )
         ORDER BY u.id",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
}

#[test]
fn test_subquery_with_group_by_having_order_limit() {
    let ctx = TestContext::new("subquery_group_having");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Subquery with GROUP BY and HAVING: user_ids with total order count >= 2 (only Alice, id=1)
    let res = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id IN (
             SELECT user_id FROM orders WHERE user_id IS NOT NULL GROUP BY user_id HAVING COUNT(id) >= 2
         )",
    )
    .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));

    // Scalar subquery with ORDER BY and LIMIT 1
    let res2 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id = (
             SELECT user_id FROM orders WHERE user_id IS NOT NULL ORDER BY amount DESC LIMIT 1
         )",
    )
    .unwrap();
    // Highest amount is 150.0 from user 2 (Bob)
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("Bob".into()));
}

#[test]
fn test_dml_with_subqueries() {
    let ctx = TestContext::new("dml_subqueries");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();

    // DELETE users who have no orders
    let del_res = txn
        .execute_statement(
            "DELETE FROM users WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = users.id)",
        )
        .unwrap();
    assert_eq!(del_res.rows_affected(), 2);

    let users_rem = query_txn(&mut txn, "SELECT id FROM users ORDER BY id").unwrap();
    assert_eq!(users_rem.rows.len(), 2);
    assert_eq!(users_rem.rows[0].values[0], Value::UInt64(1));
    assert_eq!(users_rem.rows[1].values[0], Value::UInt64(2));

    // UPDATE users balance who have pending orders
    let upd_res = txn
        .execute_statement(
            "UPDATE users SET balance = 0.0 WHERE id IN (SELECT user_id FROM orders WHERE status = 'pending')",
        )
        .unwrap();
    assert_eq!(upd_res.rows_affected(), 1);

    let bob_res = query_txn(&mut txn, "SELECT balance FROM users WHERE id = 2").unwrap();
    assert_eq!(bob_res.rows[0].values[0], Value::Float64(0.0));

    txn.commit().unwrap();
}

#[test]
fn test_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_vis_rollback");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Insert new order for Charlie (id=3) uncommitted
    txn.execute_statement(
        "INSERT INTO orders (id, user_id, amount, status) VALUES (50, 3, 500.0, 'completed')",
    )
    .unwrap();

    // Subquery should immediately see the uncommitted order for Charlie!
    let res1 = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id AND orders.id = 50)",
    )
    .unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[0], Value::String("Charlie".into()));

    // Abort transaction
    txn.abort().unwrap();

    // Verify after abort that subquery no longer sees order 50
    let mut txn2 = db.begin().unwrap();
    let res2 = query_txn(
        &mut txn2,
        "SELECT name FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id AND orders.id = 50)",
    )
    .unwrap();
    assert_eq!(res2.rows.len(), 0);
}

#[test]
fn test_persistence_across_close_reopen() {
    let ctx = TestContext::new("persistence_subqueries");
    {
        let mut db = ctx.db();
        setup_ecommerce_db(&mut db);
    }
    // Reopen DB
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        let res = query_txn(
            &mut txn,
            "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id",
        )
        .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
        assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
    }
}

#[test]
fn test_optimizer_and_explain_subquery() {
    let ctx = TestContext::new("explain_subquery");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // 1. Scalar correlated subquery (retains Correlated Subquery node)
    let explanation_scalar = txn
        .explain_sql("SELECT u.name FROM users u WHERE u.id = (SELECT o.user_id FROM orders o WHERE o.user_id = u.id LIMIT 1)")
        .unwrap();

    assert!(
        explanation_scalar.contains("Correlated Subquery"),
        "Explanation must show Correlated Subquery:\n{explanation_scalar}"
    );
    assert!(
        explanation_scalar.contains("Outer references: [u.id]"),
        "Explanation must show Outer references: [u.id]:\n{explanation_scalar}"
    );

    // 2. EXISTS subquery (decorrelated to Semi Join)
    let explanation_exists = txn
        .explain_sql("SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)")
        .unwrap();
    assert!(
        explanation_exists.contains("Semi"),
        "Explanation must show Semi Join for decorrelated EXISTS:\n{explanation_exists}"
    );
}

#[test]
fn test_uncorrelated_subquery_access_path_equivalence() {
    let ctx = TestContext::new("uncorr_access_path");
    let mut db = ctx.db();
    setup_ecommerce_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Subquery using primary key lookup vs full table scan should yield same results
    let res_pk = query_txn(
        &mut txn,
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE id = 10)",
    )
    .unwrap();
    assert_eq!(res_pk.rows.len(), 1);
    assert_eq!(res_pk.rows[0].values[0], Value::String("Alice".into()));
}

// =========================================================================
// 40. Randomized 3VL Subquery Oracle Test
// =========================================================================

#[test]
fn test_randomized_3vl_subquery_oracle() {
    let ctx = TestContext::new("randomized_3vl_oracle");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema_outer = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("t_outer", schema_outer).unwrap();

    let schema_inner = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("item", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("t_inner", schema_inner).unwrap();

    // Pseudo-random generation of test values with NULLs, duplicates, empty sets
    let mut seed: u64 = 0xDEADBEEFCAFE1234;
    let mut lcg = || -> u64 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    let sample_pool: [Option<i64>; 6] = [None, Some(10), Some(20), Some(30), Some(40), None];

    for iteration in 0..15 {
        // Clear tables
        let _ = txn.execute_statement("DELETE FROM t_outer");
        let _ = txn.execute_statement("DELETE FROM t_inner");

        let outer_count = (lcg() % 6) as usize; // 0 to 5 rows
        let inner_count = (lcg() % 6) as usize; // 0 to 5 rows

        let mut outer_vals: Vec<(u64, Option<i64>)> = Vec::new();
        for i in 0..outer_count {
            let choice = sample_pool[(lcg() % (sample_pool.len() as u64)) as usize];
            outer_vals.push((i as u64 + 1, choice));
            let val_str = match choice {
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            txn.execute_statement(&format!(
                "INSERT INTO t_outer (id, val) VALUES ({}, {})",
                i + 1,
                val_str
            ))
            .unwrap();
        }

        let mut inner_vals: Vec<Option<i64>> = Vec::new();
        for i in 0..inner_count {
            let choice = sample_pool[(lcg() % (sample_pool.len() as u64)) as usize];
            inner_vals.push(choice);
            let val_str = match choice {
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            txn.execute_statement(&format!(
                "INSERT INTO t_inner (id, item) VALUES ({}, {})",
                i + 1,
                val_str
            ))
            .unwrap();
        }

        // 1. Test IN semantics against pure reference oracle
        let mut expected_in_ids = Vec::new();
        for (id, val_opt) in &outer_vals {
            let keep = match val_opt {
                None => false,
                Some(v) => {
                    let mut matched = false;
                    for inner_v in inner_vals.iter().flatten() {
                        if *inner_v == *v {
                            matched = true;
                            break;
                        }
                    }
                    matched
                }
            };
            if keep {
                expected_in_ids.push(*id);
            }
        }

        let in_query_res = query_txn(
            &mut txn,
            "SELECT id FROM t_outer WHERE val IN (SELECT item FROM t_inner) ORDER BY id",
        )
        .unwrap();
        let actual_in_ids: Vec<u64> = in_query_res
            .rows
            .iter()
            .map(|r| match r.values[0] {
                Value::UInt64(v) => v,
                _ => panic!("expected UInt64"),
            })
            .collect();
        assert_eq!(
            actual_in_ids, expected_in_ids,
            "IN oracle mismatch on iteration {iteration}:\nouter: {outer_vals:?}\ninner: {inner_vals:?}"
        );

        // 2. Test NOT IN semantics against pure reference oracle
        let mut expected_not_in_ids = Vec::new();
        for (id, val_opt) in &outer_vals {
            let keep = if inner_vals.is_empty() {
                true
            } else {
                match val_opt {
                    None => false,
                    Some(v) => {
                        let mut has_null = false;
                        let mut matched = false;
                        for iv in &inner_vals {
                            match iv {
                                Some(inner_v) => {
                                    if *inner_v == *v {
                                        matched = true;
                                        break;
                                    }
                                }
                                None => has_null = true,
                            }
                        }
                        !matched && !has_null
                    }
                }
            };
            if keep {
                expected_not_in_ids.push(*id);
            }
        }

        let not_in_query_res = query_txn(
            &mut txn,
            "SELECT id FROM t_outer WHERE val NOT IN (SELECT item FROM t_inner) ORDER BY id",
        )
        .unwrap();
        let actual_not_in_ids: Vec<u64> = not_in_query_res
            .rows
            .iter()
            .map(|r| match r.values[0] {
                Value::UInt64(v) => v,
                _ => panic!("expected UInt64"),
            })
            .collect();
        assert_eq!(
            actual_not_in_ids, expected_not_in_ids,
            "NOT IN oracle mismatch on iteration {iteration}:\nouter: {outer_vals:?}\ninner: {inner_vals:?}"
        );

        // 3. Test EXISTS semantics against reference oracle
        let expected_exists_ids: Vec<u64> = if inner_vals.is_empty() {
            Vec::new()
        } else {
            outer_vals.iter().map(|(id, _)| *id).collect()
        };

        let exists_query_res = query_txn(
            &mut txn,
            "SELECT id FROM t_outer WHERE EXISTS (SELECT item FROM t_inner) ORDER BY id",
        )
        .unwrap();
        let actual_exists_ids: Vec<u64> = exists_query_res
            .rows
            .iter()
            .map(|r| match r.values[0] {
                Value::UInt64(v) => v,
                _ => panic!("expected UInt64"),
            })
            .collect();
        assert_eq!(
            actual_exists_ids, expected_exists_ids,
            "EXISTS oracle mismatch on iteration {iteration}"
        );

        // 4. Test NOT EXISTS semantics against reference oracle
        let expected_not_exists_ids: Vec<u64> = if inner_vals.is_empty() {
            outer_vals.iter().map(|(id, _)| *id).collect()
        } else {
            Vec::new()
        };

        let not_exists_query_res = query_txn(
            &mut txn,
            "SELECT id FROM t_outer WHERE NOT EXISTS (SELECT item FROM t_inner) ORDER BY id",
        )
        .unwrap();
        let actual_not_exists_ids: Vec<u64> = not_exists_query_res
            .rows
            .iter()
            .map(|r| match r.values[0] {
                Value::UInt64(v) => v,
                _ => panic!("expected UInt64"),
            })
            .collect();
        assert_eq!(
            actual_not_exists_ids, expected_not_exists_ids,
            "NOT EXISTS oracle mismatch on iteration {iteration}"
        );
    }

    txn.commit().unwrap();
}
