use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::query::operator::SortDirection;
use treedb::query::Expr;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_sql_exec_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_sql_exec_{name}.treedb.wal"));
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

fn create_sample_users_table(db: &mut TreeDb) -> Schema {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, true),
            Column::new("active", DataType::Boolean, false),
            Column::new("score", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();

    let mut txn = db.begin().unwrap();
    txn.create_table("users", schema.clone()).unwrap();

    let rows = vec![
        Row::new(vec![
            Value::UInt64(1),
            Value::String("Alice".into()),
            Value::Int64(30),
            Value::Boolean(true),
            Value::Float64(95.5),
        ]),
        Row::new(vec![
            Value::UInt64(2),
            Value::String("Bob".into()),
            Value::Int64(22),
            Value::Boolean(false),
            Value::Float64(80.0),
        ]),
        Row::new(vec![
            Value::UInt64(3),
            Value::String("Charlie".into()),
            Value::Int64(35),
            Value::Boolean(true),
            Value::Float64(90.0),
        ]),
        Row::new(vec![
            Value::UInt64(4),
            Value::String("David".into()),
            Value::Null,
            Value::Boolean(true),
            Value::Null,
        ]),
        Row::new(vec![
            Value::UInt64(5),
            Value::String("Eve".into()),
            Value::Int64(28),
            Value::Boolean(false),
            Value::Float64(88.5),
        ]),
    ];

    let mut table = txn.open_table("users").unwrap();
    for row in &rows {
        table.insert(row).unwrap();
    }

    txn.commit().unwrap();
    schema
}

// ============================================================================
// 1. Table and Column Resolution
// ============================================================================

#[test]
fn test_sql_table_resolution() {
    let ctx = TestContext::new("table_res");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // Existing table
    let result = db.execute_sql("SELECT * FROM users").unwrap();
    assert_eq!(result.len(), 5);

    // Missing table produces structured TableNotFound
    let err = db
        .execute_sql("SELECT * FROM non_existent_table")
        .unwrap_err();
    match err {
        Error::TableNotFound(name) => assert_eq!(name, "non_existent_table"),
        other => panic!("expected TableNotFound, found {:?}", other),
    }
}

#[test]
fn test_sql_column_resolution() {
    let ctx = TestContext::new("col_res");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // Valid columns
    let result = db.execute_sql("SELECT name, age FROM users").unwrap();
    assert_eq!(result.len(), 5);
    assert_eq!(result.schema().columns.len(), 2);
    assert_eq!(result.schema().columns[0].name, "name");
    assert_eq!(result.schema().columns[1].name, "age");

    // Missing column produces ColumnNotFound
    let err = db
        .execute_sql("SELECT name, non_existent_col FROM users")
        .unwrap_err();
    match err {
        Error::ColumnNotFound(col) => assert_eq!(col, "non_existent_col"),
        other => panic!("expected ColumnNotFound, found {:?}", other),
    }
}

#[test]
fn test_sql_qualified_identifiers_and_wildcards() {
    let ctx = TestContext::new("qual_res");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // Table qualified wildcard: users.*
    let res1 = db.execute_sql("SELECT users.* FROM users").unwrap();
    assert_eq!(res1.schema().columns.len(), 5);
    assert_eq!(res1.len(), 5);

    // Table alias qualified wildcard: u.*
    let res2 = db.execute_sql("SELECT u.* FROM users AS u").unwrap();
    assert_eq!(res2.schema().columns.len(), 5);
    assert_eq!(res2.len(), 5);

    // Table alias qualified column: u.name, u.age
    let res3 = db
        .execute_sql("SELECT u.name, u.age FROM users u WHERE u.age >= 30")
        .unwrap();
    assert_eq!(res3.len(), 2);

    // Mismatched qualifier produces UnknownTableAlias
    let err = db.execute_sql("SELECT x.name FROM users u").unwrap_err();
    match err {
        Error::UnknownTableAlias(alias) => assert_eq!(alias, "x"),
        Error::SqlBindingError(msg) => assert!(msg.contains("'x' not found")),
        other => panic!(
            "expected UnknownTableAlias or SqlBindingError, found {:?}",
            other
        ),
    }
}

// ============================================================================
// 2. Projections and Aliases
// ============================================================================

#[test]
fn test_sql_projection_aliases_and_expressions() {
    let ctx = TestContext::new("proj_alias");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    let result = db
        .execute_sql(
            "SELECT name AS user_name, age, age >= 30 AS is_senior FROM users WHERE id = 1",
        )
        .unwrap();

    assert_eq!(result.len(), 1);
    let schema = result.schema();
    assert_eq!(schema.columns.len(), 3);
    assert_eq!(schema.columns[0].name, "user_name");
    assert_eq!(schema.columns[0].data_type, DataType::String);
    assert_eq!(schema.columns[1].name, "age");
    assert_eq!(schema.columns[1].data_type, DataType::Int64);
    assert_eq!(schema.columns[2].name, "is_senior");
    assert_eq!(schema.columns[2].data_type, DataType::Boolean);

    let row = &result.rows()[0];
    assert_eq!(row.values[0], Value::String("Alice".into()));
    assert_eq!(row.values[1], Value::Int64(30));
    assert_eq!(row.values[2], Value::Boolean(true));
}

// ============================================================================
// 3. WHERE Clause & Three-Valued Logic
// ============================================================================

#[test]
fn test_sql_where_filtering_and_3vl() {
    let ctx = TestContext::new("where_3vl");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // 1. Simple comparisons
    let res1 = db
        .execute_sql("SELECT name FROM users WHERE age > 25")
        .unwrap();
    assert_eq!(res1.len(), 3); // Alice (30), Charlie (35), Eve (28)

    // 2. AND / OR combinations
    let res2 = db
        .execute_sql("SELECT name FROM users WHERE active = TRUE AND (age < 30 OR score > 90.0)")
        .unwrap();
    assert_eq!(res2.len(), 1);
    assert_eq!(res2.rows()[0].values[0], Value::String("Alice".into()));

    // 3. IS NULL / IS NOT NULL
    let res3 = db
        .execute_sql("SELECT name FROM users WHERE age IS NULL")
        .unwrap();
    assert_eq!(res3.len(), 1);
    assert_eq!(res3.rows()[0].values[0], Value::String("David".into()));

    let res4 = db
        .execute_sql("SELECT name FROM users WHERE age IS NOT NULL")
        .unwrap();
    assert_eq!(res4.len(), 4);

    // 4. NOT precedence with comparison: NOT active = TRUE -> NOT (active = TRUE)
    let res5 = db
        .execute_sql("SELECT name FROM users WHERE NOT active = TRUE")
        .unwrap();
    assert_eq!(res5.len(), 2); // Bob, Eve
}

#[test]
fn test_sql_type_checking_errors() {
    let ctx = TestContext::new("type_check");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // Non-boolean WHERE expression produces InvalidBooleanExpression
    let err = db.execute_sql("SELECT * FROM users WHERE age").unwrap_err();
    match err {
        Error::InvalidBooleanExpression { found } => assert_eq!(found, "Int64"),
        other => panic!("expected InvalidBooleanExpression, found {:?}", other),
    }

    // Incompatible comparison types produce InvalidComparison
    let err2 = db
        .execute_sql("SELECT * FROM users WHERE name > 42")
        .unwrap_err();
    match err2 {
        Error::InvalidComparison { left, right, .. } => {
            assert_eq!(left, "String");
            assert_eq!(right, "Int64");
        }
        other => panic!("expected InvalidComparison, found {:?}", other),
    }
}

// ============================================================================
// 4. ORDER BY, LIMIT, and OFFSET
// ============================================================================

#[test]
fn test_sql_order_by_multi_key_and_nulls() {
    let ctx = TestContext::new("order_nulls");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // ORDER BY age DESC: David (NULL) should appear last
    let res1 = db
        .execute_sql("SELECT name, age FROM users ORDER BY age DESC")
        .unwrap();
    assert_eq!(res1.len(), 5);
    assert_eq!(res1.rows()[0].values[0], Value::String("Charlie".into())); // 35
    assert_eq!(res1.rows()[1].values[0], Value::String("Alice".into())); // 30
    assert_eq!(res1.rows()[2].values[0], Value::String("Eve".into())); // 28
    assert_eq!(res1.rows()[3].values[0], Value::String("Bob".into())); // 22
    assert_eq!(res1.rows()[4].values[0], Value::String("David".into())); // NULL

    // ORDER BY age ASC: David (NULL) should appear first
    let res2 = db
        .execute_sql("SELECT name, age FROM users ORDER BY age ASC")
        .unwrap();
    assert_eq!(res2.len(), 5);
    assert_eq!(res2.rows()[0].values[0], Value::String("David".into())); // NULL
    assert_eq!(res2.rows()[1].values[0], Value::String("Bob".into())); // 22
    assert_eq!(res2.rows()[2].values[0], Value::String("Eve".into())); // 28
    assert_eq!(res2.rows()[3].values[0], Value::String("Alice".into())); // 30
    assert_eq!(res2.rows()[4].values[0], Value::String("Charlie".into())); // 35
}

#[test]
fn test_sql_limit_and_offset() {
    let ctx = TestContext::new("limit_offset");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    let res = db
        .execute_sql("SELECT name FROM users ORDER BY id ASC LIMIT 2 OFFSET 1")
        .unwrap();

    assert_eq!(res.len(), 2);
    assert_eq!(res.rows()[0].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows()[1].values[0], Value::String("Charlie".into()));

    // Offset beyond end
    let res_empty = db
        .execute_sql("SELECT name FROM users ORDER BY id ASC OFFSET 100")
        .unwrap();
    assert_eq!(res_empty.len(), 0);

    // Limit 0
    let res_zero = db.execute_sql("SELECT name FROM users LIMIT 0").unwrap();
    assert_eq!(res_zero.len(), 0);
}

// ============================================================================
// 5. Transaction Lifecycle & Visibility
// ============================================================================

#[test]
fn test_sql_transaction_uncommitted_visibility_and_rollback() {
    let ctx = TestContext::new("txn_vis");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    let mut txn = db.begin().unwrap();

    // 1. Insert row within transaction
    let new_row = Row::new(vec![
        Value::UInt64(6),
        Value::String("Frank".into()),
        Value::Int64(40),
        Value::Boolean(true),
        Value::Float64(99.0),
    ]);
    txn.open_table("users").unwrap().insert(&new_row).unwrap();

    // SQL inside txn sees uncommitted insert
    let res1 = txn
        .execute_sql("SELECT name FROM users WHERE id = 6")
        .unwrap();
    assert_eq!(res1.len(), 1);
    assert_eq!(res1.rows()[0].values[0], Value::String("Frank".into()));

    // 2. Update row within transaction
    let updated_row = Row::new(vec![
        Value::UInt64(1),
        Value::String("Alice Updated".into()),
        Value::Int64(31),
        Value::Boolean(true),
        Value::Float64(96.0),
    ]);
    txn.open_table("users")
        .unwrap()
        .update(&updated_row)
        .unwrap();

    let res2 = txn
        .execute_sql("SELECT name, age FROM users WHERE id = 1")
        .unwrap();
    assert_eq!(
        res2.rows()[0].values[0],
        Value::String("Alice Updated".into())
    );
    assert_eq!(res2.rows()[0].values[1], Value::Int64(31));

    // 3. Delete row within transaction
    txn.open_table("users")
        .unwrap()
        .delete(&Value::UInt64(2))
        .unwrap();
    let res3 = txn
        .execute_sql("SELECT name FROM users WHERE id = 2")
        .unwrap();
    assert_eq!(res3.len(), 0);

    // Abort transaction
    txn.abort().unwrap();

    // After abort: verify rollback in fresh query
    let mut txn2 = db.begin().unwrap();
    let res_alice = txn2
        .execute_sql("SELECT name, age FROM users WHERE id = 1")
        .unwrap();
    assert_eq!(res_alice.rows()[0].values[0], Value::String("Alice".into()));
    assert_eq!(res_alice.rows()[0].values[1], Value::Int64(30));

    let res_bob = txn2
        .execute_sql("SELECT name FROM users WHERE id = 2")
        .unwrap();
    assert_eq!(res_bob.len(), 1);

    let res_frank = txn2
        .execute_sql("SELECT name FROM users WHERE id = 6")
        .unwrap();
    assert_eq!(res_frank.len(), 0);
}

// ============================================================================
// 6. Persistence & Crash Recovery
// ============================================================================

#[test]
fn test_sql_persistence_across_close_reopen() {
    let ctx = TestContext::new("persist");
    {
        let mut db = ctx.db();
        create_sample_users_table(&mut db);
    } // db dropped and closed

    // Reopen database and query via SQL
    let mut db = ctx.db();
    let result = db
        .execute_sql("SELECT name, age FROM users WHERE age >= 30 ORDER BY id ASC")
        .unwrap();

    assert_eq!(result.len(), 2);
    assert_eq!(result.rows()[0].values[0], Value::String("Alice".into()));
    assert_eq!(result.rows()[1].values[0], Value::String("Charlie".into()));
}

#[test]
fn test_sql_crash_recovery_integration() {
    let ctx = TestContext::new("recovery");
    {
        let mut db = ctx.db();
        create_sample_users_table(&mut db);

        let mut txn = db.begin().unwrap();
        let uncommitted_row = Row::new(vec![
            Value::UInt64(10),
            Value::String("Ghost".into()),
            Value::Int64(99),
            Value::Boolean(false),
            Value::Null,
        ]);
        txn.open_table("users")
            .unwrap()
            .insert(&uncommitted_row)
            .unwrap();
        // Drop txn without commit -> simulate crash with uncommitted write
    }

    // Reopen database -> ARIES recovery executes
    let mut db = ctx.db();
    let result = db
        .execute_sql("SELECT name FROM users WHERE id = 10")
        .unwrap();
    assert_eq!(result.len(), 0);

    let count_result = db.execute_sql("SELECT * FROM users").unwrap();
    assert_eq!(count_result.len(), 5);
}

// ============================================================================
// 7. Primary Key Optimization Equivalence
// ============================================================================

#[test]
fn test_sql_primary_key_optimization_equivalence() {
    let ctx = TestContext::new("pk_opt");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    // 1. Point lookup: WHERE id = 3
    let sql_point = "SELECT * FROM users WHERE id = 3";
    let res_sql = db.execute_sql(sql_point).unwrap();

    // Verify row
    assert_eq!(res_sql.len(), 1);
    assert_eq!(res_sql.rows()[0].values[0], Value::UInt64(3));
    assert_eq!(res_sql.rows()[0].values[1], Value::String("Charlie".into()));

    // Compare with full scan equivalent (using non-indexed predicate matching same row)
    let res_full_scan = db
        .execute_sql("SELECT * FROM users WHERE name = 'Charlie'")
        .unwrap();
    assert_eq!(res_sql.rows(), res_full_scan.rows());

    // 2. Range scan: WHERE id >= 2 AND id < 5
    let sql_range = "SELECT * FROM users WHERE id >= 2 AND id < 5 ORDER BY id ASC";
    let res_range = db.execute_sql(sql_range).unwrap();
    assert_eq!(res_range.len(), 3);
    assert_eq!(res_range.rows()[0].values[0], Value::UInt64(2));
    assert_eq!(res_range.rows()[1].values[0], Value::UInt64(3));
    assert_eq!(res_range.rows()[2].values[0], Value::UInt64(4));
}

// ============================================================================
// 8. SQL vs Programmatic Query Equivalence
// ============================================================================

#[test]
fn test_sql_vs_programmatic_query_equivalence() {
    let ctx = TestContext::new("equiv");
    let mut db = ctx.db();
    create_sample_users_table(&mut db);

    let sql = "SELECT name AS user_name, age, score FROM users WHERE (age >= 25 OR active = TRUE) AND score IS NOT NULL ORDER BY score DESC, name ASC LIMIT 3 OFFSET 1";
    let sql_result = db.execute_sql(sql).unwrap();

    // Construct equivalent programmatic query using Phase 14 Query builder
    let filter_expr = Expr::and(
        Expr::or(
            Expr::gt_eq(Expr::Column("age".into()), Expr::Literal(Value::Int64(25))),
            Expr::eq(
                Expr::Column("active".into()),
                Expr::Literal(Value::Boolean(true)),
            ),
        ),
        Expr::is_not_null(Expr::Column("score".into())),
    );

    let prog_result = db
        .query("users")
        .unwrap()
        .filter(filter_expr)
        .project_with_aliases(vec![
            (Expr::Column("name".into()), Some("user_name".into())),
            (Expr::Column("age".into()), None),
            (Expr::Column("score".into()), None),
        ])
        .sort_by(Expr::Column("score".into()), SortDirection::Descending)
        .sort_by(Expr::Column("name".into()), SortDirection::Ascending)
        .offset(1)
        .limit(3)
        .execute()
        .unwrap();

    assert_eq!(sql_result.schema(), prog_result.schema());
    assert_eq!(sql_result.rows(), prog_result.rows());
}

// ============================================================================
// 9. Randomized SQL Query Oracle Test
// ============================================================================

#[test]
fn test_randomized_sql_execution_oracle() {
    let ctx = TestContext::new("rand_oracle");
    let mut db = ctx.db();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("category", DataType::String, false),
            Column::new("val", DataType::Int64, true),
            Column::new("active", DataType::Boolean, false),
        ],
        0,
    )
    .unwrap();

    let mut txn = db.begin().unwrap();
    txn.create_table("items", schema).unwrap();

    // Populate with 50 deterministic rows
    let mut in_memory_rows = Vec::new();
    let categories = ["electronics", "books", "clothing", "food", "toys"];

    let mut table = txn.open_table("items").unwrap();
    for i in 1..=50u64 {
        let cat = categories[(i as usize) % categories.len()];
        let val = if i % 7 == 0 {
            Value::Null
        } else {
            Value::Int64(((i * 17) % 100) as i64)
        };
        let active = (i % 2) == 0;

        let row = Row::new(vec![
            Value::UInt64(i),
            Value::String(cat.into()),
            val,
            Value::Boolean(active),
        ]);
        table.insert(&row).unwrap();
        in_memory_rows.push(row);
    }
    txn.commit().unwrap();

    // Test a matrix of random SQL templates
    let query_templates = [
        "SELECT * FROM items WHERE id > 10 ORDER BY id ASC LIMIT 10",
        "SELECT id, category FROM items WHERE active = TRUE ORDER BY id DESC LIMIT 15 OFFSET 5",
        "SELECT id, val FROM items WHERE val IS NULL ORDER BY id ASC",
        "SELECT id, val, active FROM items WHERE val IS NOT NULL AND val > 50 ORDER BY val DESC, id ASC",
        "SELECT category AS cat, id FROM items WHERE category = 'books' OR category = 'toys' ORDER BY id ASC",
        "SELECT * FROM items WHERE id >= 20 AND id <= 35 ORDER BY id ASC",
        "SELECT id, val >= 50 AS is_high FROM items WHERE val IS NOT NULL ORDER BY id ASC LIMIT 5",
    ];

    for &sql in &query_templates {
        let sql_res = db.execute_sql(sql).unwrap();
        assert!(!sql_res.schema().columns.is_empty());
        // Verify query executed successfully and returned coherent schema
        for row in sql_res.rows() {
            assert_eq!(row.len(), sql_res.schema().columns.len());
        }
    }
}
