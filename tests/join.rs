use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

struct TestContext {
    path: std::path::PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "treedb_join_{}_{}_{}.db",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let wal_path = path.with_extension("wal");
        if wal_path.exists() {
            let _ = std::fs::remove_file(&wal_path);
        }
        Self { path }
    }

    fn db(&self) -> TreeDb {
        if self.path.exists() {
            TreeDb::open(&self.path, 64).unwrap()
        } else {
            TreeDb::create(&self.path, 64).unwrap()
        }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("wal"));
    }
}

fn create_users_and_orders_schema() -> (Schema, Schema, Schema) {
    let users_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("department", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let orders_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("user_id", DataType::Int64, true),
            Column::new("amount", DataType::Float64, false),
            Column::new("status", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let items_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("order_id", DataType::Int64, false),
            Column::new("item_name", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    (users_schema, orders_schema, items_schema)
}

// ============================================================================
// 1. INNER JOIN Tests
// ============================================================================

#[test]
fn test_inner_join_basic_and_cardinality() {
    let ctx = TestContext::new("inner_basic");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::String("Sales".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Charlie".into()),
                Value::String("HR".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        // Alice has 2 orders
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(50.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(102),
                Value::Int64(1),
                Value::Float64(150.0),
                Value::String("pending".into()),
            ]))
            .unwrap();
        // Bob has 1 order
        orders
            .insert(&Row::new(vec![
                Value::Int64(103),
                Value::Int64(2),
                Value::Float64(200.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        // Order for non-existent user 99
        orders
            .insert(&Row::new(vec![
                Value::Int64(104),
                Value::Int64(99),
                Value::Float64(300.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // 1. Basic INNER JOIN
    let res = txn
        .execute_sql("SELECT u.name, o.id, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id ORDER BY o.id ASC;")
        .unwrap();

    assert_eq!(res.len(), 3);
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(101),
            Value::Float64(50.0)
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(102),
            Value::Float64(150.0)
        ]
    );
    assert_eq!(
        res.rows[2].values,
        vec![
            Value::String("Bob".into()),
            Value::Int64(103),
            Value::Float64(200.0)
        ]
    );

    // 2. JOIN shorthand
    let res2 = txn
        .execute_sql(
            "SELECT u.name, o.id FROM users u JOIN orders o ON u.id = o.user_id ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res2.len(), 3);

    // 3. Compound ON predicate
    let res3 = txn
        .execute_sql("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id AND o.status = 'paid' ORDER BY o.amount ASC;")
        .unwrap();
    assert_eq!(res3.len(), 2);
    assert_eq!(
        res3.rows[0].values,
        vec![Value::String("Alice".into()), Value::Float64(50.0)]
    );
    assert_eq!(
        res3.rows[1].values,
        vec![Value::String("Bob".into()), Value::Float64(200.0)]
    );
}

// ============================================================================
// 2. LEFT OUTER JOIN Tests
// ============================================================================

#[test]
fn test_left_join_basic_and_null_padding() {
    let ctx = TestContext::new("left_basic");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::String("Sales".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Charlie".into()),
                Value::String("HR".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(50.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // LEFT JOIN: Alice (has order 101), Bob (NULL), Charlie (NULL)
    let res = txn
        .execute_sql("SELECT u.name, o.id, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY u.id ASC;")
        .unwrap();

    assert_eq!(res.len(), 3);
    // Alice
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(101),
            Value::Float64(50.0)
        ]
    );
    // Bob (NULL padded)
    assert_eq!(
        res.rows[1].values,
        vec![Value::String("Bob".into()), Value::Null, Value::Null]
    );
    // Charlie (NULL padded)
    assert_eq!(
        res.rows[2].values,
        vec![Value::String("Charlie".into()), Value::Null, Value::Null]
    );

    // LEFT OUTER JOIN syntax
    let res2 = txn
        .execute_sql("SELECT u.name, o.id FROM users u LEFT OUTER JOIN orders o ON u.id = o.user_id ORDER BY u.id ASC;")
        .unwrap();
    assert_eq!(res2.len(), 3);
    assert_eq!(res2.rows[1].values[1], Value::Null);
    assert_eq!(res2.rows[2].values[1], Value::Null);
}

// ============================================================================
// 3. NULL Join Key Semantics (3VL)
// ============================================================================

#[test]
fn test_join_null_key_semantics() {
    let ctx = TestContext::new("join_null_3vl");
    let mut db = ctx.db();

    let schema_a = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let schema_b = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("a_val", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("tbl_a", schema_a).unwrap();
    txn.create_table("tbl_b", schema_b).unwrap();

    {
        let mut a = txn.open_table("tbl_a").unwrap();
        a.insert(&Row::new(vec![Value::Int64(1), Value::String("X".into())]))
            .unwrap();
        a.insert(&Row::new(vec![Value::Int64(2), Value::Null]))
            .unwrap();

        let mut b = txn.open_table("tbl_b").unwrap();
        b.insert(&Row::new(vec![Value::Int64(10), Value::String("X".into())]))
            .unwrap();
        b.insert(&Row::new(vec![Value::Int64(20), Value::Null]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // INNER JOIN: NULL = NULL does NOT match under 3VL
    let res_inner = txn
        .execute_sql("SELECT a.id, b.id FROM tbl_a a JOIN tbl_b b ON a.val = b.a_val;")
        .unwrap();
    assert_eq!(res_inner.len(), 1);
    assert_eq!(
        res_inner.rows[0].values,
        vec![Value::Int64(1), Value::Int64(10)]
    );

    // LEFT JOIN: a.id=2 (NULL val) does not match b.id=20, gets NULL padded
    let res_left = txn
        .execute_sql(
            "SELECT a.id, b.id FROM tbl_a a LEFT JOIN tbl_b b ON a.val = b.a_val ORDER BY a.id ASC;",
        )
        .unwrap();
    assert_eq!(res_left.len(), 2);
    assert_eq!(
        res_left.rows[0].values,
        vec![Value::Int64(1), Value::Int64(10)]
    );
    assert_eq!(res_left.rows[1].values, vec![Value::Int64(2), Value::Null]);
}

// ============================================================================
// 4. Multiple Chained Joins
// ============================================================================

#[test]
fn test_multiple_chained_joins() {
    let ctx = TestContext::new("multi_join");
    let mut db = ctx.db();
    let (u_schema, o_schema, i_schema) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();
    txn.create_table("items", i_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::String("Sales".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(50.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(102),
                Value::Int64(2),
                Value::Float64(75.0),
                Value::String("paid".into()),
            ]))
            .unwrap();

        let mut items = txn.open_table("items").unwrap();
        items
            .insert(&Row::new(vec![
                Value::Int64(1001),
                Value::Int64(101),
                Value::String("Keyboard".into()),
            ]))
            .unwrap();
        items
            .insert(&Row::new(vec![
                Value::Int64(1002),
                Value::Int64(101),
                Value::String("Mouse".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // 3-table join: users -> orders -> items
    let res = txn
        .execute_sql(
            "SELECT u.name, o.id, i.item_name
             FROM users u
             JOIN orders o ON u.id = o.user_id
             LEFT JOIN items i ON o.id = i.order_id
             ORDER BY u.name ASC, i.item_name ASC;",
        )
        .unwrap();

    assert_eq!(res.len(), 3);
    // Alice has 2 items
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(101),
            Value::String("Keyboard".into())
        ]
    );
    assert_eq!(
        res.rows[1].values,
        vec![
            Value::String("Alice".into()),
            Value::Int64(101),
            Value::String("Mouse".into())
        ]
    );
    // Bob has order 102 with no items -> NULL item_name
    assert_eq!(
        res.rows[2].values,
        vec![Value::String("Bob".into()), Value::Int64(102), Value::Null]
    );
}

// ============================================================================
// 5. Binding, Aliases, Ambiguity, and Wildcards
// ============================================================================

#[test]
fn test_join_aliases_and_ambiguity() {
    let ctx = TestContext::new("join_binding");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(50.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // 1. Ambiguous column: both users and orders have 'id'
    let err = txn
        .execute_sql("SELECT id FROM users JOIN orders ON users.id = orders.user_id;")
        .unwrap_err();
    assert!(matches!(err, Error::AmbiguousColumn(c) if c == "id"));

    // 2. Unambiguous columns: 'name' is in users, 'amount' is in orders
    let res = txn
        .execute_sql("SELECT name, amount FROM users JOIN orders ON users.id = orders.user_id;")
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(
        res.rows[0].values,
        vec![Value::String("Alice".into()), Value::Float64(50.0)]
    );

    // 3. Unknown table alias
    let err2 = txn
        .execute_sql("SELECT x.id FROM users u JOIN orders o ON u.id = o.user_id;")
        .unwrap_err();
    assert!(matches!(err2, Error::UnknownTableAlias(a) if a == "x"));

    // 4. Column not found in table
    let err3 = txn
        .execute_sql("SELECT u.nonexistent FROM users u JOIN orders o ON u.id = o.user_id;")
        .unwrap_err();
    assert!(matches!(err3, Error::ColumnNotFound(_)));

    // 5. Wildcard expansion: SELECT * expands left-to-right to all columns
    let res_wildcard = txn
        .execute_sql("SELECT * FROM users u JOIN orders o ON u.id = o.user_id;")
        .unwrap();
    assert_eq!(res_wildcard.schema().columns.len(), 7); // 3 users + 4 orders
    assert_eq!(
        res_wildcard.rows[0].values,
        vec![
            Value::Int64(1),
            Value::String("Alice".into()),
            Value::String("Eng".into()),
            Value::Int64(101),
            Value::Int64(1),
            Value::Float64(50.0),
            Value::String("paid".into()),
        ]
    );

    // 6. Qualified wildcards: SELECT u.*, o.amount
    let res_qual_wild = txn
        .execute_sql("SELECT u.*, o.amount FROM users u JOIN orders o ON u.id = o.user_id;")
        .unwrap();
    assert_eq!(res_qual_wild.schema().columns.len(), 4);
    assert_eq!(
        res_qual_wild.rows[0].values,
        vec![
            Value::Int64(1),
            Value::String("Alice".into()),
            Value::String("Eng".into()),
            Value::Float64(50.0),
        ]
    );
}

// ============================================================================
// 6. JOIN + WHERE + ORDER BY + LIMIT / OFFSET
// ============================================================================

#[test]
fn test_join_with_filtering_sorting_and_pagination() {
    let ctx = TestContext::new("join_pipeline");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        for i in 1..=5 {
            users
                .insert(&Row::new(vec![
                    Value::Int64(i),
                    Value::String(format!("User_{i}")),
                    Value::String(if i % 2 == 0 { "Eng" } else { "Sales" }.into()),
                ]))
                .unwrap();
        }

        let mut orders = txn.open_table("orders").unwrap();
        for i in 1..=10 {
            orders
                .insert(&Row::new(vec![
                    Value::Int64(100 + i),
                    Value::Int64((i % 5) + 1),
                    Value::Float64(i as f64 * 10.0),
                    Value::String(if i % 3 == 0 { "cancelled" } else { "paid" }.into()),
                ]))
                .unwrap();
        }
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // JOIN + WHERE + ORDER BY + LIMIT + OFFSET
    let res = txn
        .execute_sql(
            "SELECT u.name, o.id AS order_id, o.amount
             FROM users u
             JOIN orders o ON u.id = o.user_id
             WHERE o.status = 'paid' AND u.department = 'Sales'
             ORDER BY o.amount DESC
             LIMIT 2 OFFSET 1;",
        )
        .unwrap();

    assert_eq!(res.len(), 2);
    assert_eq!(res.schema().columns[1].name, "order_id");
}

// ============================================================================
// 7. JOIN + GROUP BY + HAVING + Aggregations
// ============================================================================

#[test]
fn test_join_with_group_by_having_and_aggregates() {
    let ctx = TestContext::new("join_agg");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Charlie".into()),
                Value::String("Sales".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(100.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(102),
                Value::Int64(1),
                Value::Float64(200.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(103),
                Value::Int64(2),
                Value::Float64(300.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // Group by department with SUM and COUNT
    let res = txn
        .execute_sql(
            "SELECT u.department, COUNT(o.id) AS order_count, SUM(o.amount) AS total_amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.department
             ORDER BY u.department ASC;",
        )
        .unwrap();

    assert_eq!(res.len(), 2);
    // Eng: Alice (2 orders, 300.0) + Bob (1 order, 300.0) -> 3 orders, 600.0
    assert_eq!(
        res.rows[0].values,
        vec![
            Value::String("Eng".into()),
            Value::UInt64(3),
            Value::Float64(600.0)
        ]
    );
    // Sales: Charlie (0 orders) -> 0 orders, NULL total_amount
    assert_eq!(
        res.rows[1].values,
        vec![Value::String("Sales".into()), Value::UInt64(0), Value::Null]
    );

    // HAVING clause: filter out departments with 0 orders
    let res_having = txn
        .execute_sql(
            "SELECT u.department, COUNT(o.id) AS order_count
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.department
             HAVING COUNT(o.id) > 0;",
        )
        .unwrap();

    assert_eq!(res_having.len(), 1);
    assert_eq!(
        res_having.rows[0].values,
        vec![Value::String("Eng".into()), Value::UInt64(3)]
    );
}

// ============================================================================
// 8. Critical COUNT(*) vs COUNT(column) Distinction on LEFT JOIN
// ============================================================================

#[test]
fn test_left_join_count_star_vs_count_column_distinction() {
    let ctx = TestContext::new("count_distinction");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        // User with no orders
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("SoloUser".into()),
                Value::String("None".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // Query unmatched left row:
    // COUNT(*) counts the NULL-padded joined row (yielding 1)
    // COUNT(o.id) sees Value::Null (yielding 0)
    let res = txn
        .execute_sql(
            "SELECT u.id, COUNT(*), COUNT(o.id), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount)
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.id;",
        )
        .unwrap();

    assert_eq!(res.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::UInt64(1)); // COUNT(*) == 1
    assert_eq!(res.rows[0].values[2], Value::UInt64(0)); // COUNT(o.id) == 0
    assert_eq!(res.rows[0].values[3], Value::Null); // SUM == NULL
    assert_eq!(res.rows[0].values[4], Value::Null); // AVG == NULL
    assert_eq!(res.rows[0].values[5], Value::Null); // MIN == NULL
    assert_eq!(res.rows[0].values[6], Value::Null); // MAX == NULL
}

// ============================================================================
// 9. Transaction Visibility & Abort Rollback
// ============================================================================

#[test]
fn test_join_transaction_uncommitted_visibility_and_rollback() {
    let ctx = TestContext::new("join_txn");
    let mut db = ctx.db();
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", u_schema).unwrap();
    txn.create_table("orders", o_schema).unwrap();

    {
        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(50.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }
    txn.commit().unwrap();

    // 1. Uncommitted insert is visible
    let mut txn2 = db.begin_transaction().unwrap();
    {
        let mut users = txn2.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::String("Sales".into()),
            ]))
            .unwrap();

        let mut orders = txn2.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(102),
                Value::Int64(2),
                Value::Float64(100.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
    }

    let res_uncommitted = txn2
        .execute_sql(
            "SELECT u.name, o.id FROM users u JOIN orders o ON u.id = o.user_id ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res_uncommitted.len(), 2);

    // 2. Abort restores previous state
    txn2.abort().unwrap();

    let mut txn3 = db.begin_transaction().unwrap();
    let res_aborted = txn3
        .execute_sql(
            "SELECT u.name, o.id FROM users u JOIN orders o ON u.id = o.user_id ORDER BY o.id ASC;",
        )
        .unwrap();
    assert_eq!(res_aborted.len(), 1);
    assert_eq!(
        res_aborted.rows[0].values,
        vec![Value::String("Alice".into()), Value::Int64(101)]
    );
}

// ============================================================================
// 10. Persistence and Recovery
// ============================================================================

#[test]
fn test_join_persistence_and_recovery() {
    let ctx = TestContext::new("join_persist");
    let (u_schema, o_schema, _) = create_users_and_orders_schema();

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("users", u_schema).unwrap();
        txn.create_table("orders", o_schema).unwrap();

        let mut users = txn.open_table("users").unwrap();
        users
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::String("Eng".into()),
            ]))
            .unwrap();

        let mut orders = txn.open_table("orders").unwrap();
        orders
            .insert(&Row::new(vec![
                Value::Int64(101),
                Value::Int64(1),
                Value::Float64(75.0),
                Value::String("paid".into()),
            ]))
            .unwrap();
        txn.commit().unwrap();
        db.sync().unwrap();
    }

    // Reopen and verify JOIN query
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        let res = txn
            .execute_sql("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;")
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(
            res.rows[0].values,
            vec![Value::String("Alice".into()), Value::Float64(75.0)]
        );
    }
}

// ============================================================================
// 11. Randomized Reference Oracle
// ============================================================================

#[test]
fn test_join_randomized_reference_oracle() {
    let ctx = TestContext::new("join_oracle");
    let mut db = ctx.db();

    let schema_a = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("key_a", DataType::Int64, true),
            Column::new("val_a", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let schema_b = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("key_b", DataType::Int64, true),
            Column::new("val_b", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("table_a", schema_a).unwrap();
    txn.create_table("table_b", schema_b).unwrap();

    #[derive(Debug, Clone)]
    struct RowA {
        id: i64,
        key: Option<i64>,
        val: String,
    }

    #[derive(Debug, Clone)]
    struct RowB {
        id: i64,
        key: Option<i64>,
        val: i64,
    }

    let mut ref_a = Vec::new();
    let mut ref_b = Vec::new();

    {
        let mut table_a = txn.open_table("table_a").unwrap();
        for i in 1..=30 {
            let key = if i % 6 == 0 { None } else { Some((i * 7) % 10) };
            let val = format!("A_{i}");
            ref_a.push(RowA {
                id: i,
                key,
                val: val.clone(),
            });
            table_a
                .insert(&Row::new(vec![
                    Value::Int64(i),
                    key.map(Value::Int64).unwrap_or(Value::Null),
                    Value::String(val),
                ]))
                .unwrap();
        }

        let mut table_b = txn.open_table("table_b").unwrap();
        for j in 1..=40 {
            let key = if j % 5 == 0 { None } else { Some((j * 3) % 10) };
            let val = j * 10;
            ref_b.push(RowB { id: j, key, val });
            table_b
                .insert(&Row::new(vec![
                    Value::Int64(j),
                    key.map(Value::Int64).unwrap_or(Value::Null),
                    Value::Int64(val),
                ]))
                .unwrap();
        }
    }
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();

    // 1. Reference INNER JOIN
    let mut ref_inner_rows: Vec<(i64, i64, String, i64)> = Vec::new();
    for a in &ref_a {
        if let Some(ka) = a.key {
            for b in &ref_b {
                if let Some(kb) = b.key {
                    if ka == kb {
                        ref_inner_rows.push((a.id, b.id, a.val.clone(), b.val));
                    }
                }
            }
        }
    }
    ref_inner_rows.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));

    let sql_inner = "SELECT a.id, b.id, a.val_a, b.val_b FROM table_a a JOIN table_b b ON a.key_a = b.key_b ORDER BY a.id ASC, b.id ASC;";
    let res_inner = txn.execute_sql(sql_inner).unwrap();
    assert_eq!(res_inner.len(), ref_inner_rows.len());

    for (idx, row) in res_inner.rows.iter().enumerate() {
        let expected = &ref_inner_rows[idx];
        assert_eq!(row.values[0], Value::Int64(expected.0));
        assert_eq!(row.values[1], Value::Int64(expected.1));
        assert_eq!(row.values[2], Value::String(expected.2.clone()));
        assert_eq!(row.values[3], Value::Int64(expected.3));
    }

    // 2. Reference LEFT JOIN
    let mut ref_left_rows: Vec<(i64, Option<i64>, String, Option<i64>)> = Vec::new();
    for a in &ref_a {
        let mut matched = false;
        if let Some(ka) = a.key {
            for b in &ref_b {
                if let Some(kb) = b.key {
                    if ka == kb {
                        matched = true;
                        ref_left_rows.push((a.id, Some(b.id), a.val.clone(), Some(b.val)));
                    }
                }
            }
        }
        if !matched {
            ref_left_rows.push((a.id, None, a.val.clone(), None));
        }
    }
    ref_left_rows.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));

    let sql_left = "SELECT a.id, b.id, a.val_a, b.val_b FROM table_a a LEFT JOIN table_b b ON a.key_a = b.key_b ORDER BY a.id ASC, b.id ASC;";
    let res_left = txn.execute_sql(sql_left).unwrap();
    assert_eq!(res_left.len(), ref_left_rows.len());

    for (idx, row) in res_left.rows.iter().enumerate() {
        let expected = &ref_left_rows[idx];
        assert_eq!(row.values[0], Value::Int64(expected.0));
        assert_eq!(
            row.values[1],
            expected.1.map(Value::Int64).unwrap_or(Value::Null)
        );
        assert_eq!(row.values[2], Value::String(expected.2.clone()));
        assert_eq!(
            row.values[3],
            expected.3.map(Value::Int64).unwrap_or(Value::Null)
        );
    }
}
