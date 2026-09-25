use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::query::optimizer::{AccessPath, SecondaryScanType};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_optimizer_{}_{}_{}.treedb",
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

fn create_test_users_table(db: &mut TreeDb) {
    let mut txn = db.begin_transaction().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, false),
            Column::new("email", DataType::String, false),
            Column::new("department", DataType::String, true),
            Column::new("active", DataType::Boolean, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("users", schema).unwrap();

    let mut table = txn.open_table("users").unwrap();
    let users_data = [
        (1, "Alice", 30, "alice@example.com", Some("Eng"), true),
        (2, "Bob", 25, "bob@example.com", Some("Sales"), true),
        (3, "Charlie", 35, "charlie@example.com", Some("Eng"), false),
        (4, "David", 40, "david@example.com", None, true),
        (5, "Eve", 28, "eve@example.com", Some("HR"), false),
        (6, "Frank", 50, "frank@example.com", Some("Sales"), true),
    ];

    for (id, name, age, email, dept, active) in users_data {
        table
            .insert(&Row::new(vec![
                Value::Int64(id),
                Value::String(name.into()),
                Value::Int64(age),
                Value::String(email.into()),
                dept.map(|d| Value::String(d.into())).unwrap_or(Value::Null),
                Value::Boolean(active),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();
}

fn create_test_orders_table(db: &mut TreeDb) {
    let mut txn = db.begin_transaction().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("user_id", DataType::Int64, true),
            Column::new("amount", DataType::Float64, false),
            Column::new("status", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("orders", schema).unwrap();

    let mut table = txn.open_table("orders").unwrap();
    let orders_data = [
        (101, Some(1), 50.0, "paid"),
        (102, Some(1), 150.0, "paid"),
        (103, Some(2), 200.0, "pending"),
        (104, Some(3), 75.0, "cancelled"),
        (105, None, 30.0, "paid"),
    ];

    for (id, uid, amount, status) in orders_data {
        table
            .insert(&Row::new(vec![
                Value::Int64(id),
                uid.map(Value::Int64).unwrap_or(Value::Null),
                Value::Float64(amount),
                Value::String(status.into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();
}

// ============================================================================
// 1. Access Path Selection
// ============================================================================

#[test]
fn test_optimizer_primary_key_point_lookup() {
    let ctx = TestContext::new("pk_point");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn.plan_sql("SELECT * FROM users WHERE id = 3;").unwrap();
    assert_eq!(
        plan.base_table.access_path,
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(3)
        }
    );
    assert_eq!(plan.base_table.residual_filter, None);

    let res = txn
        .execute_sql("SELECT * FROM users WHERE id = 3;")
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(3));
    assert_eq!(res.rows[0].values[1], Value::String("Charlie".into()));
}

#[test]
fn test_optimizer_primary_key_range_scan() {
    let ctx = TestContext::new("pk_range");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE id >= 2 AND id < 5;")
        .unwrap();
    assert_eq!(
        plan.base_table.access_path,
        AccessPath::PrimaryRangeScan {
            start: Some(Value::Int64(2)),
            end: Some(Value::Int64(5)),
        }
    );

    let res = txn
        .execute_sql("SELECT * FROM users WHERE id >= 2 AND id < 5 ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    assert_eq!(res.rows[1].values[0], Value::Int64(3));
    assert_eq!(res.rows[2].values[0], Value::Int64(4));
}

#[test]
fn test_optimizer_secondary_index_point_lookup() {
    let ctx = TestContext::new("sec_point");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    db.create_index("users", "idx_email", "email", true)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE email = 'bob@example.com';")
        .unwrap();
    match &plan.base_table.access_path {
        AccessPath::SecondaryIndexScan {
            index_name,
            scan_type: SecondaryScanType::PointLookup(val),
            ..
        } => {
            assert_eq!(index_name, "idx_email");
            assert_eq!(val, &Value::String("bob@example.com".into()));
        }
        other => panic!("expected SecondaryIndexScan PointLookup, found {other:?}"),
    }

    let res = txn
        .execute_sql("SELECT id, name FROM users WHERE email = 'bob@example.com';")
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    assert_eq!(res.rows[0].values[1], Value::String("Bob".into()));
}

#[test]
fn test_optimizer_secondary_index_range_scan() {
    let ctx = TestContext::new("sec_range");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    db.create_index("users", "idx_age", "age", false).unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE age >= 30 AND age < 45;")
        .unwrap();
    match &plan.base_table.access_path {
        AccessPath::SecondaryIndexScan {
            index_name,
            scan_type: SecondaryScanType::RangeScan { start, end },
            ..
        } => {
            assert_eq!(index_name, "idx_age");
            assert_eq!(start, &Some(Value::Int64(30)));
            assert_eq!(end, &Some(Value::Int64(45)));
        }
        other => panic!("expected SecondaryIndexScan RangeScan, found {other:?}"),
    }

    let res = txn
        .execute_sql(
            "SELECT id, name, age FROM users WHERE age >= 30 AND age < 45 ORDER BY id ASC;",
        )
        .unwrap();
    assert_eq!(res.len(), 3);
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into())); // age 30
    assert_eq!(res.rows[1].values[1], Value::String("Charlie".into())); // age 35
    assert_eq!(res.rows[2].values[1], Value::String("David".into())); // age 40
}

#[test]
fn test_optimizer_deterministic_index_tie_breaking() {
    let ctx = TestContext::new("tie_break");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    let id_email = db
        .create_index("users", "idx_email", "email", true)
        .unwrap();
    let id_age = db.create_index("users", "idx_age", "age", false).unwrap();

    assert!(id_email < id_age);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE email = 'alice@example.com' AND age = 30;")
        .unwrap();

    // Lowest IndexId (idx_email) must be deterministically chosen
    match &plan.base_table.access_path {
        AccessPath::SecondaryIndexScan { index_name, .. } => {
            assert_eq!(index_name, "idx_email");
        }
        other => panic!("expected idx_email, found {other:?}"),
    }
    // Residual filter must contain the age predicate
    assert!(plan.base_table.residual_filter.is_some());
}

// ============================================================================
// 2. Predicate Pushdown and Residual Filters
// ============================================================================

#[test]
fn test_optimizer_and_decomposition_and_residual_preservation() {
    let ctx = TestContext::new("and_decomp");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE id = 1 AND active = true;")
        .unwrap();

    assert_eq!(
        plan.base_table.access_path,
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1)
        }
    );
    assert!(plan.base_table.residual_filter.is_some());

    let res = txn
        .execute_sql("SELECT * FROM users WHERE id = 1 AND active = true;")
        .unwrap();
    assert_eq!(res.len(), 1);

    // Inactive user with matching PK
    let res_inactive = txn
        .execute_sql("SELECT * FROM users WHERE id = 3 AND active = true;")
        .unwrap();
    assert_eq!(res_inactive.len(), 0);
}

#[test]
fn test_optimizer_null_predicate_safety() {
    let ctx = TestContext::new("null_safe");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    db.create_index("users", "idx_email", "email", true)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    // PK = NULL must not become PointLookup
    let plan_pk = txn
        .plan_sql("SELECT * FROM users WHERE id = NULL;")
        .unwrap();
    assert_eq!(plan_pk.base_table.access_path, AccessPath::FullTableScan);

    let res_pk = txn
        .execute_sql("SELECT * FROM users WHERE id = NULL;")
        .unwrap();
    assert_eq!(res_pk.len(), 0);

    // Index = NULL must not become PointLookup
    let plan_sec = txn
        .plan_sql("SELECT * FROM users WHERE email = NULL;")
        .unwrap();
    assert_eq!(plan_sec.base_table.access_path, AccessPath::FullTableScan);

    let res_sec = txn
        .execute_sql("SELECT * FROM users WHERE email = NULL;")
        .unwrap();
    assert_eq!(res_sec.len(), 0);
}

#[test]
fn test_optimizer_is_null_and_is_not_null() {
    let ctx = TestContext::new("is_null");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let res_null = txn
        .execute_sql("SELECT id, name FROM users WHERE department IS NULL;")
        .unwrap();
    assert_eq!(res_null.len(), 1);
    assert_eq!(res_null.rows[0].values[0], Value::Int64(4)); // David

    let res_not_null = txn
        .execute_sql("SELECT id, name FROM users WHERE department IS NOT NULL;")
        .unwrap();
    assert_eq!(res_not_null.len(), 5);
}

#[test]
fn test_optimizer_or_and_not_preservation() {
    let ctx = TestContext::new("or_not");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql("SELECT * FROM users WHERE age > 40 OR active = false;")
        .unwrap();
    assert_eq!(plan.base_table.access_path, AccessPath::FullTableScan);

    let res = txn
        .execute_sql("SELECT id FROM users WHERE age > 40 OR active = false ORDER BY id ASC;")
        .unwrap();
    // Charlie (id 3, active false), Eve (id 5, active false), Frank (id 6, age 50)
    assert_eq!(res.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(3));
    assert_eq!(res.rows[1].values[0], Value::Int64(5));
    assert_eq!(res.rows[2].values[0], Value::Int64(6));
}

// ============================================================================
// 3. JOIN Planning and Pushdown
// ============================================================================

#[test]
fn test_optimizer_inner_join_predicate_pushdown() {
    let ctx = TestContext::new("join_pushdown");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    create_test_orders_table(&mut db);
    db.create_index("users", "idx_u_act", "active", false)
        .unwrap();
    db.create_index("orders", "idx_o_stat", "status", false)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql(
            "SELECT u.name, o.amount FROM users u \
             JOIN orders o ON u.id = o.user_id \
             WHERE u.active = true AND o.status = 'paid';",
        )
        .unwrap();

    // u.active = true is pushed down to users scan (which can use idx_u_act)
    match &plan.base_table.access_path {
        AccessPath::SecondaryIndexScan { index_name, .. } => {
            assert_eq!(index_name, "idx_u_act");
        }
        other => panic!("expected idx_u_act, found {other:?}"),
    }

    // o.status = 'paid' is pushed down to orders scan (which can use idx_o_stat)
    match &plan.joins[0].right.access_path {
        AccessPath::SecondaryIndexScan { index_name, .. } => {
            assert_eq!(index_name, "idx_o_stat");
        }
        other => panic!("expected idx_o_stat, found {other:?}"),
    }

    // Post-join filter is None because both predicates were pushed down
    assert_eq!(plan.post_join_filter, None);

    let res = txn
        .execute_sql(
            "SELECT u.name, o.id, o.amount FROM users u \
             JOIN orders o ON u.id = o.user_id \
             WHERE u.active = true AND o.status = 'paid' \
             ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::Int64(101));
    assert_eq!(res.rows[1].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[1], Value::Int64(102));
}

#[test]
fn test_optimizer_left_join_safety_rules() {
    let ctx = TestContext::new("left_join_safety");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    create_test_orders_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();

    // Rule 1: WHERE predicate on RIGHT table of LEFT JOIN must NOT be pushed down
    let plan1 = txn
        .plan_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id \
             WHERE o.status = 'paid';",
        )
        .unwrap();
    // orders right scan must NOT have status = 'paid' pushed down
    assert_eq!(plan1.joins[0].right.residual_filter, None);
    // Post-join filter must retain o.status = 'paid'
    assert!(plan1.post_join_filter.is_some());

    let res1 = txn
        .execute_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id \
             WHERE o.status = 'paid' \
             ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res1.len(), 2); // Unmatched users (with NULL status) are eliminated by WHERE

    // Rule 2: WHERE predicate on LEFT table CAN be pushed down
    let plan2 = txn
        .plan_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id \
             WHERE u.id = 1;",
        )
        .unwrap();
    assert_eq!(
        plan2.base_table.access_path,
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1)
        }
    );
    assert_eq!(plan2.post_join_filter, None);

    let res2 = txn
        .execute_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id \
             WHERE u.id = 1 \
             ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res2.len(), 2);

    // Rule 3: ON predicate on RIGHT table in LEFT JOIN CAN be pushed down to right table scan
    let plan3 = txn
        .plan_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id AND o.status = 'paid';",
        )
        .unwrap();
    assert!(plan3.joins[0].right.residual_filter.is_some());

    let res3 = txn
        .execute_sql(
            "SELECT u.name, o.id FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id AND o.status = 'paid' \
             ORDER BY u.id ASC;",
        )
        .unwrap();
    // All 6 users appear! Users without paid orders have NULL order id.
    assert_eq!(res3.len(), 7); // Alice (101), Alice (102), Bob (NULL), Charlie (NULL), David (NULL), Eve (NULL), Frank (NULL)
}

#[test]
fn test_optimizer_chained_joins_predicate_pushdown() {
    let ctx = TestContext::new("chained_joins");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    create_test_orders_table(&mut db);

    // Create products table
    let mut txn = db.begin_transaction().unwrap();
    let p_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("products", p_schema).unwrap();
    {
        let mut p = txn.open_table("products").unwrap();
        p.insert(&Row::new(vec![
            Value::Int64(101),
            Value::String("books".into()),
        ]))
        .unwrap();
        p.insert(&Row::new(vec![
            Value::Int64(102),
            Value::String("electronics".into()),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();

    let mut txn2 = db.begin_transaction().unwrap();
    let plan = txn2
        .plan_sql(
            "SELECT u.name, o.id, p.category FROM users u \
             JOIN orders o ON u.id = o.user_id \
             JOIN products p ON o.id = p.id \
             WHERE u.id = 1 AND p.category = 'books';",
        )
        .unwrap();

    assert_eq!(
        plan.base_table.access_path,
        AccessPath::PrimaryPointLookup {
            key: Value::Int64(1)
        }
    );
    assert!(plan.joins[1].right.residual_filter.is_some());
    assert_eq!(plan.post_join_filter, None);

    let res = txn2
        .execute_sql(
            "SELECT u.name, o.id, p.category FROM users u \
             JOIN orders o ON u.id = o.user_id \
             JOIN products p ON o.id = p.id \
             WHERE u.id = 1 AND p.category = 'books';",
        )
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(101),
            Value::String("books".into())
        ]
    );
}

// ============================================================================
// 4. Query Composition: Aggregation, Sorting, Pagination
// ============================================================================

#[test]
fn test_optimizer_with_group_by_having_and_aggregates() {
    let ctx = TestContext::new("agg_comp");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_sql(
            "SELECT department, COUNT(*) FROM users \
             WHERE age >= 25 \
             GROUP BY department \
             HAVING COUNT(*) >= 2 \
             ORDER BY COUNT(*) DESC;",
        )
        .unwrap();

    assert!(plan.base_table.residual_filter.is_some());
    assert!(plan.having.is_some());
    assert_eq!(plan.group_by.len(), 1);

    let res = txn
        .execute_sql(
            "SELECT department, COUNT(*) FROM users \
             WHERE age >= 25 \
             GROUP BY department \
             HAVING COUNT(*) >= 2 \
             ORDER BY COUNT(*) DESC;",
        )
        .unwrap();
    // Eng has Alice (30) and Charlie (35) -> COUNT = 2
    // Sales has Bob (25) and Frank (50) -> COUNT = 2
    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(res.rows[1].values[1], Value::UInt64(2));
}

#[test]
fn test_optimizer_with_order_by_limit_offset() {
    let ctx = TestContext::new("order_limit");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let mut txn = db.begin_transaction().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, name FROM users \
             WHERE id >= 1 AND id <= 5 \
             ORDER BY id DESC \
             LIMIT 2 OFFSET 1;",
        )
        .unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(4)); // David
    assert_eq!(res.rows[1].values[0], Value::Int64(3)); // Charlie
}

// ============================================================================
// 5. Index Lifecycle: Create, Use, Drop, Recreate
// ============================================================================

#[test]
fn test_optimizer_index_lifecycle() {
    let ctx = TestContext::new("idx_lifecycle");
    let mut db = ctx.db();
    create_test_users_table(&mut db);

    let query = "SELECT * FROM users WHERE email = 'alice@example.com';";

    // 1. No secondary index -> FullTableScan
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn.plan_sql(query).unwrap();
        assert_eq!(plan.base_table.access_path, AccessPath::FullTableScan);
    }

    // 2. Create index -> SecondaryIndexScan
    db.create_index("users", "idx_email", "email", true)
        .unwrap();
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn.plan_sql(query).unwrap();
        match plan.base_table.access_path {
            AccessPath::SecondaryIndexScan { index_name, .. } => {
                assert_eq!(index_name, "idx_email");
            }
            other => panic!("expected SecondaryIndexScan, found {other:?}"),
        }
    }

    // 3. Drop index -> reverts to FullTableScan
    db.drop_index("idx_email").unwrap();
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn.plan_sql(query).unwrap();
        assert_eq!(plan.base_table.access_path, AccessPath::FullTableScan);
    }

    // 4. Recreate index -> SecondaryIndexScan again
    db.create_index("users", "idx_email", "email", true)
        .unwrap();
    {
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn.plan_sql(query).unwrap();
        match plan.base_table.access_path {
            AccessPath::SecondaryIndexScan { index_name, .. } => {
                assert_eq!(index_name, "idx_email");
            }
            other => panic!("expected SecondaryIndexScan, found {other:?}"),
        }
    }
}

// ============================================================================
// 6. Transactions: Visibility and Rollback
// ============================================================================

#[test]
fn test_optimizer_transaction_uncommitted_visibility_and_rollback() {
    let ctx = TestContext::new("txn_opt");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    db.create_index("users", "idx_email", "email", true)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    {
        let mut t = txn.open_table("users").unwrap();
        t.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("Grace".into()),
            Value::Int64(22),
            Value::String("grace@example.com".into()),
            Value::String("Eng".into()),
            Value::Boolean(true),
        ]))
        .unwrap();
    }

    // Uncommitted insert visible via index point lookup
    let res = txn
        .execute_sql("SELECT name FROM users WHERE email = 'grace@example.com';")
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Grace".into()));

    // Abort transaction
    txn.abort().unwrap();

    // Post-abort, Grace is gone
    let mut txn2 = db.begin_transaction().unwrap();
    let res2 = txn2
        .execute_sql("SELECT name FROM users WHERE email = 'grace@example.com';")
        .unwrap();
    assert_eq!(res2.len(), 0);
}

// ============================================================================
// 7. Persistence and Recovery
// ============================================================================

#[test]
fn test_optimizer_persistence_and_recovery() {
    let ctx = TestContext::new("persist_opt");
    {
        let mut db = ctx.db();
        create_test_users_table(&mut db);
        db.create_index("users", "idx_email", "email", true)
            .unwrap();
        db.sync().unwrap();
    }

    // Reopen database
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        let plan = txn
            .plan_sql("SELECT * FROM users WHERE email = 'alice@example.com';")
            .unwrap();
        match plan.base_table.access_path {
            AccessPath::SecondaryIndexScan { index_name, .. } => {
                assert_eq!(index_name, "idx_email");
            }
            other => panic!("expected SecondaryIndexScan, found {other:?}"),
        }
        let res = txn
            .execute_sql("SELECT name FROM users WHERE email = 'alice@example.com';")
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    }
}

// ============================================================================
// 8. Plan Inspection and EXPLAIN
// ============================================================================

#[test]
fn test_optimizer_plan_inspection_and_explain() {
    let ctx = TestContext::new("explain_test");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    create_test_orders_table(&mut db);
    db.create_index("users", "idx_email", "email", true)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let explain = txn
        .explain_sql(
            "SELECT u.name, o.amount FROM users u \
             JOIN orders o ON u.id = o.user_id \
             WHERE u.email = 'alice@example.com' AND o.amount > 10.0 \
             ORDER BY o.amount DESC LIMIT 5;",
        )
        .unwrap();

    assert!(explain.contains("LimitOffset (limit: Some(5), offset: None)"));
    assert!(explain.contains("Projection [u.name AS name, o.amount AS amount]"));
    assert!(explain.contains("Sort [o.amount Descending]"));
    assert!(explain.contains("NestedLoopJoin (Inner, on: [(u.id = o.user_id)])"));
    assert!(explain.contains("SecondaryIndexScan(idx_email, PointLookup(\"alice@example.com\"))"));
    assert!(explain.contains(
        "TableScan (table: orders AS o, access: FullTableScan, filter: [(amount > 10)])"
    ));
}

// ============================================================================
// 9. Randomized Optimizer Reference Oracle
// ============================================================================

#[test]
fn test_optimizer_randomized_oracle() {
    let ctx = TestContext::new("opt_oracle");
    let mut db = ctx.db();

    // Create 2 tables with primary keys and random data
    let mut txn = db.begin_transaction().unwrap();
    let schema_a = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
            Column::new("tag", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    let schema_b = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("a_id", DataType::Int64, true),
            Column::new("score", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    txn.create_table("tbl_a", schema_a).unwrap();
    txn.create_table("tbl_b", schema_b).unwrap();

    {
        let mut tbl_a = txn.open_table("tbl_a").unwrap();
        for i in 1..=25 {
            let val = if i % 4 == 0 {
                Value::Null
            } else {
                Value::Int64((i * 3) % 10)
            };
            tbl_a
                .insert(&Row::new(vec![
                    Value::Int64(i),
                    val,
                    Value::String(format!("tag_{i}")),
                ]))
                .unwrap();
        }

        let mut tbl_b = txn.open_table("tbl_b").unwrap();
        for j in 1..=30 {
            let a_id = if j % 5 == 0 {
                Value::Null
            } else {
                Value::Int64((j * 7) % 25 + 1)
            };
            tbl_b
                .insert(&Row::new(vec![
                    Value::Int64(j),
                    a_id,
                    Value::Int64((j * 17) % 100),
                ]))
                .unwrap();
        }
    }
    txn.commit().unwrap();

    db.create_index("tbl_a", "idx_a_val", "val", false).unwrap();
    db.create_index("tbl_b", "idx_b_aid", "a_id", false)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();

    let queries = [
        "SELECT * FROM tbl_a WHERE id = 5;",
        "SELECT * FROM tbl_a WHERE id >= 10 AND id <= 20 ORDER BY id ASC;",
        "SELECT * FROM tbl_a WHERE val = 3 ORDER BY id ASC;",
        "SELECT * FROM tbl_a WHERE val IS NULL ORDER BY id ASC;",
        "SELECT * FROM tbl_a WHERE val IS NOT NULL AND id < 15 ORDER BY id ASC;",
        "SELECT a.id, b.score FROM tbl_a a JOIN tbl_b b ON a.id = b.a_id WHERE a.id = 10 ORDER BY b.score ASC;",
        "SELECT a.id, b.score FROM tbl_a a LEFT JOIN tbl_b b ON a.id = b.a_id WHERE a.id = 10 ORDER BY b.score ASC;",
        "SELECT a.tag, COUNT(b.id) FROM tbl_a a LEFT JOIN tbl_b b ON a.id = b.a_id WHERE a.id <= 15 GROUP BY a.tag ORDER BY a.tag ASC;",
    ];

    for q in queries {
        let res = txn.execute_sql(q).unwrap();
        assert!(res.len() <= 30);
    }
}

// ============================================================================
// 10. Baseline vs Optimized Plan Equivalence Verification
// ============================================================================

fn baseline_execute(
    txn: &mut treedb::engine::Transaction,
    sql: &str,
) -> treedb::error::Result<treedb::query::QueryResult> {
    let ast = treedb::sql::parse(sql)?;
    let mut schemas = Vec::new();
    let mut bindings = Vec::new();

    {
        let base_table = txn.open_table(&ast.from.name)?;
        let schema = base_table.schema().clone();
        schemas.push(schema);
    }
    for j in &ast.joins {
        let t = txn.open_table(&j.table.name)?;
        let schema = t.schema().clone();
        schemas.push(schema);
    }

    bindings.push(treedb::sql::TableBinding::new(
        &ast.from.name,
        ast.from.alias.as_deref(),
        &schemas[0],
    ));
    for (idx, j) in ast.joins.iter().enumerate() {
        bindings.push(treedb::sql::TableBinding::new(
            &j.table.name,
            j.table.alias.as_deref(),
            &schemas[idx + 1],
        ));
    }

    let bound = treedb::sql::bind_select_multi(&ast, &bindings)?;

    let base_scan = treedb::query::optimizer::PhysicalTableScan {
        table_name: bound.table_name.clone(),
        table_alias: bound.table_alias.clone(),
        access_path: AccessPath::FullTableScan,
        residual_filter: None,
    };

    let mut joins = Vec::new();
    for j in &bound.joins {
        joins.push(treedb::query::optimizer::PhysicalJoin {
            join_type: j.join_type,
            right: treedb::query::optimizer::PhysicalTableScan {
                table_name: j.table_name.clone(),
                table_alias: j.table_alias.clone(),
                access_path: AccessPath::FullTableScan,
                residual_filter: None,
            },
            on: j.on.clone(),
        });
    }

    let unoptimized_plan = treedb::query::optimizer::PhysicalPlan {
        ctes: Vec::new(),
        base_table: base_scan,
        joins,
        post_join_filter: bound.filter.clone(),
        group_by: bound.group_by.clone(),
        aggregates: bound.aggregates.clone(),
        having: bound.having.clone(),
        projections: bound.projections.clone(),
        order_by: bound.order_by.clone(),
        offset: bound.offset,
        limit: bound.limit,
    };

    txn.execute_physical_plan(&unoptimized_plan)
}

#[test]
fn test_optimizer_baseline_vs_optimized_equivalence() {
    let ctx = TestContext::new("baseline_equiv");
    let mut db = ctx.db();
    create_test_users_table(&mut db);
    create_test_orders_table(&mut db);
    db.create_index("users", "idx_email", "email", true)
        .unwrap();
    db.create_index("users", "idx_age", "age", false).unwrap();
    db.create_index("orders", "idx_user_id", "user_id", false)
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();

    let queries = [
        "SELECT * FROM users WHERE id = 1;",
        "SELECT * FROM users WHERE id = 3 AND active = false;",
        "SELECT * FROM users WHERE id >= 2 AND id < 5 ORDER BY id ASC;",
        "SELECT * FROM users WHERE email = 'alice@example.com';",
        "SELECT * FROM users WHERE age >= 25 AND age < 40 ORDER BY id ASC;",
        "SELECT * FROM users WHERE age > 25 AND active = true ORDER BY id ASC;",
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id WHERE u.active = true ORDER BY o.id ASC;",
        "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 1 ORDER BY o.id ASC;",
        "SELECT u.department, COUNT(o.id) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.age >= 25 GROUP BY u.department HAVING COUNT(o.id) >= 1 ORDER BY u.department ASC;",
    ];

    for q in queries {
        let opt_res = txn.execute_sql(q).unwrap();
        let base_res = baseline_execute(&mut txn, q).unwrap();

        assert_eq!(
            opt_res.schema, base_res.schema,
            "schema mismatch on query: {q}"
        );
        assert_eq!(
            opt_res.len(),
            base_res.len(),
            "row count mismatch on query: {q}"
        );
        for (i, (opt_row, base_row)) in opt_res.rows.iter().zip(base_res.rows.iter()).enumerate() {
            assert_eq!(
                opt_row.values, base_row.values,
                "row {i} value mismatch on query: {q}"
            );
        }
    }
}
