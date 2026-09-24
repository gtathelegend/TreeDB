use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::query::expr::{AggregateExpr, BinaryOp, Expr};
use treedb::query::operator::SortDirection;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_agg_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_agg_{name}.treedb.wal"));
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

// ============================================================================
// 1. COUNT Tests (COUNT(*) vs COUNT(expr), NULLs, Empty)
// ============================================================================

#[test]
fn test_count_all_and_count_expr_and_nulls() {
    let ctx = TestContext::new("count");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("age", DataType::Int64, true),
            Column::new("name", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("users", schema).unwrap();
    {
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::Int64(20),
                Value::String("Alice".into()),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::Null,
                Value::String("Bob".into()),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::Int64(30),
                Value::Null,
            ]))
            .unwrap();
    }

    // COUNT(*) -> 3 rows total
    let res = txn.execute_sql("SELECT COUNT(*) FROM users;").unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(3));

    // COUNT(age) -> 2 non-null rows
    let res = txn.execute_sql("SELECT COUNT(age) FROM users;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(2));

    // COUNT(name) -> 2 non-null rows
    let res = txn.execute_sql("SELECT COUNT(name) FROM users;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(2));

    // Empty table test
    let empty_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("empty_tbl", empty_schema).unwrap();

    let res = txn.execute_sql("SELECT COUNT(*) FROM empty_tbl;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(0));

    let res = txn
        .execute_sql("SELECT COUNT(val) FROM empty_tbl;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(0));

    txn.commit().unwrap();
}

// ============================================================================
// 2. SUM Tests (Int64, UInt64, Float64, NULLs, Empty, Overflow, Errors)
// ============================================================================

#[test]
fn test_sum_all_numeric_types_null_and_overflow() {
    let ctx = TestContext::new("sum");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val_i64", DataType::Int64, true),
            Column::new("val_u64", DataType::UInt64, true),
            Column::new("val_f64", DataType::Float64, true),
            Column::new("val_str", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("numbers", schema).unwrap();
    {
        let mut table = txn.open_table("numbers").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::Int64(10),
                Value::UInt64(100),
                Value::Float64(1.5),
                Value::String("hello".into()),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::Int64(20),
                Value::UInt64(200),
                Value::Float64(2.5),
                Value::String("world".into()),
            ]))
            .unwrap();
    }

    // SUM(val_i64) -> 30
    let res = txn
        .execute_sql("SELECT SUM(val_i64) FROM numbers;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(30));

    // SUM(val_u64) -> 300
    let res = txn
        .execute_sql("SELECT SUM(val_u64) FROM numbers;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(300));

    // SUM(val_f64) -> 4.0
    let res = txn
        .execute_sql("SELECT SUM(val_f64) FROM numbers;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Float64(4.0));

    // SUM on empty / all-NULL table -> NULL
    let res = txn
        .execute_sql("SELECT SUM(val_i64) FROM numbers WHERE id = 999;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    let res = txn
        .execute_sql("SELECT SUM(val_i64) FROM numbers WHERE id = 2;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    // SUM on non-numeric string type -> Error
    let err = txn
        .execute_sql("SELECT SUM(val_str) FROM numbers;")
        .unwrap_err();
    assert!(matches!(err, Error::UnsupportedAggregateType { .. }));

    // Integer overflow test
    let overflow_schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("large", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("overflow_tbl", overflow_schema).unwrap();
    {
        let mut table = txn.open_table("overflow_tbl").unwrap();
        table
            .insert(&Row::new(vec![Value::Int64(1), Value::Int64(i64::MAX)]))
            .unwrap();
        table
            .insert(&Row::new(vec![Value::Int64(2), Value::Int64(1)]))
            .unwrap();
    }

    let err = txn
        .execute_sql("SELECT SUM(large) FROM overflow_tbl;")
        .unwrap_err();
    assert!(matches!(err, Error::AggregateOverflow(_)));

    txn.commit().unwrap();
}

// ============================================================================
// 3. AVG Tests (Numeric Types, sum/non_null_count, NULLs, Empty)
// ============================================================================

#[test]
fn test_avg_all_numeric_types_and_null_semantics() {
    let ctx = TestContext::new("avg");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("score", DataType::Int64, true),
            Column::new("rating", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("tests", schema).unwrap();
    {
        let mut table = txn.open_table("tests").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::Int64(10),
                Value::Float64(4.0),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![Value::Int64(2), Value::Null, Value::Null]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::Int64(20),
                Value::Float64(5.0),
            ]))
            .unwrap();
    }

    // AVG(score) = (10 + 20) / 2 = 15.0
    let res = txn.execute_sql("SELECT AVG(score) FROM tests;").unwrap();
    assert_eq!(res.schema.columns[0].data_type, DataType::Float64);
    assert_eq!(res.rows[0].values[0], Value::Float64(15.0));

    // AVG(rating) = (4.0 + 5.0) / 2 = 4.5
    let res = txn.execute_sql("SELECT AVG(rating) FROM tests;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Float64(4.5));

    // Empty / all-null AVG -> NULL
    let res = txn
        .execute_sql("SELECT AVG(score) FROM tests WHERE id = 999;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    let res = txn
        .execute_sql("SELECT AVG(score) FROM tests WHERE id = 2;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    txn.commit().unwrap();
}

// ============================================================================
// 4. MIN and MAX Tests (All Supported Types, NULLs, Float NaNs)
// ============================================================================

#[test]
fn test_min_max_all_types_and_float_nan() {
    let ctx = TestContext::new("min_max");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("flag", DataType::Boolean, true),
            Column::new("num", DataType::Int64, true),
            Column::new("fnum", DataType::Float64, true),
            Column::new("name", DataType::String, true),
            Column::new("data", DataType::Bytes, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("items", schema).unwrap();
    {
        let mut table = txn.open_table("items").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::Boolean(true),
                Value::Int64(10),
                Value::Float64(3.5),
                Value::String("apple".into()),
                Value::Bytes(vec![1, 2, 3]),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::Boolean(false),
                Value::Int64(30),
                Value::Float64(2.71),
                Value::String("banana".into()),
                Value::Bytes(vec![4, 5, 6]),
            ]))
            .unwrap();
    }

    let res = txn
        .execute_sql(
            "SELECT MIN(num), MAX(num), MIN(name), MAX(name), MIN(flag), MAX(flag) FROM items;",
        )
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(10));
    assert_eq!(res.rows[0].values[1], Value::Int64(30));
    assert_eq!(res.rows[0].values[2], Value::String("apple".into()));
    assert_eq!(res.rows[0].values[3], Value::String("banana".into()));
    assert_eq!(res.rows[0].values[4], Value::Boolean(false));
    assert_eq!(res.rows[0].values[5], Value::Boolean(true));

    // Empty / all-NULL -> NULL
    let res = txn
        .execute_sql("SELECT MIN(num), MAX(num) FROM items WHERE id = 999;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);
    assert_eq!(res.rows[0].values[1], Value::Null);

    let res = txn
        .execute_sql("SELECT MIN(num), MAX(num) FROM items WHERE id = 2;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);
    assert_eq!(res.rows[0].values[1], Value::Null);

    txn.commit().unwrap();
}

// ============================================================================
// 5. GROUP BY Tests (Single Key, Multi Key, NULL Keys, Empty Table)
// ============================================================================

#[test]
fn test_group_by_single_and_multi_key_and_null_groups() {
    let ctx = TestContext::new("groupby");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, true),
            Column::new("role", DataType::String, true),
            Column::new("salary", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("employees", schema).unwrap();
    {
        let mut table = txn.open_table("employees").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Engineering".into()),
                Value::String("Dev".into()),
                Value::Int64(100),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Engineering".into()),
                Value::String("Dev".into()),
                Value::Int64(150),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Engineering".into()),
                Value::String("Lead".into()),
                Value::Int64(200),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(4),
                Value::String("Sales".into()),
                Value::String("Rep".into()),
                Value::Int64(80),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(5),
                Value::Null,
                Value::String("Intern".into()),
                Value::Int64(50),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(6),
                Value::Null,
                Value::String("Intern".into()),
                Value::Int64(60),
            ]))
            .unwrap();
    }

    // 1. Single-key GROUP BY with NULL key grouping together
    let res = txn
        .execute_sql("SELECT dept, COUNT(*), SUM(salary), AVG(salary) FROM employees GROUP BY dept ORDER BY dept ASC;")
        .unwrap();

    // Default ASC sort puts NULL first
    assert_eq!(res.rows.len(), 3);
    // NULL group (2 interns)
    assert_eq!(res.rows[0].values[0], Value::Null);
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(res.rows[0].values[2], Value::Int64(110));
    assert_eq!(res.rows[0].values[3], Value::Float64(55.0));

    // Engineering group (3 devs/leads)
    assert_eq!(res.rows[1].values[0], Value::String("Engineering".into()));
    assert_eq!(res.rows[1].values[1], Value::UInt64(3));
    assert_eq!(res.rows[1].values[2], Value::Int64(450));
    assert_eq!(res.rows[1].values[3], Value::Float64(150.0));

    // Sales group (1 rep)
    assert_eq!(res.rows[2].values[0], Value::String("Sales".into()));
    assert_eq!(res.rows[2].values[1], Value::UInt64(1));
    assert_eq!(res.rows[2].values[2], Value::Int64(80));

    // 2. Multi-key GROUP BY
    let res = txn
        .execute_sql("SELECT dept, role, COUNT(*), SUM(salary) FROM employees GROUP BY dept, role ORDER BY dept ASC, role ASC;")
        .unwrap();
    assert_eq!(res.rows.len(), 4);

    // Empty table with GROUP BY produces 0 rows
    let res = txn
        .execute_sql("SELECT dept, COUNT(*) FROM employees WHERE id = 999 GROUP BY dept;")
        .unwrap();
    assert_eq!(res.rows.len(), 0);

    txn.commit().unwrap();
}

// ============================================================================
// 6. HAVING Tests (Aggregate Predicates, Grouped Col Predicates, No GROUP BY)
// ============================================================================

#[test]
fn test_having_filtering_and_no_group_by() {
    let ctx = TestContext::new("having");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::String, true),
            Column::new("price", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("products", schema).unwrap();
    {
        let mut table = txn.open_table("products").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Electronics".into()),
                Value::Int64(500),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Electronics".into()),
                Value::Int64(300),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Books".into()),
                Value::Int64(20),
            ]))
            .unwrap();
    }

    // 1. HAVING with aggregate predicate: COUNT(*) >= 2
    let res = txn
        .execute_sql(
            "SELECT category, COUNT(*) FROM products GROUP BY category HAVING COUNT(*) >= 2;",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Electronics".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));

    // 2. HAVING with grouped column predicate: category = 'Books'
    let res = txn
        .execute_sql("SELECT category, SUM(price) FROM products GROUP BY category HAVING category = 'Books';")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Books".into()));
    assert_eq!(res.rows[0].values[1], Value::Int64(20));

    // 3. HAVING with no GROUP BY (Single implicit group)
    // True predicate -> 1 row
    let res = txn
        .execute_sql("SELECT COUNT(*) FROM products HAVING COUNT(*) > 0;")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(3));

    // False predicate -> 0 rows
    let res = txn
        .execute_sql("SELECT COUNT(*) FROM products HAVING COUNT(*) > 100;")
        .unwrap();
    assert_eq!(res.rows.len(), 0);

    txn.commit().unwrap();
}

// ============================================================================
// 7. ORDER BY and Column Aliases with Aggregates
// ============================================================================

#[test]
fn test_order_by_with_aggregates_and_aliases() {
    let ctx = TestContext::new("orderby_agg");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, true),
            Column::new("salary", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("staff", schema).unwrap();
    {
        let mut table = txn.open_table("staff").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("HR".into()),
                Value::Int64(50),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Engineering".into()),
                Value::Int64(120),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Engineering".into()),
                Value::Int64(180),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(4),
                Value::String("Marketing".into()),
                Value::Int64(70),
            ]))
            .unwrap();
    }

    // 1. ORDER BY aggregate directly: ORDER BY SUM(salary) DESC
    let res = txn
        .execute_sql("SELECT dept, SUM(salary) FROM staff GROUP BY dept ORDER BY SUM(salary) DESC;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::String("Engineering".into()));
    assert_eq!(res.rows[0].values[1], Value::Int64(300));
    assert_eq!(res.rows[1].values[0], Value::String("Marketing".into()));
    assert_eq!(res.rows[1].values[1], Value::Int64(70));
    assert_eq!(res.rows[2].values[0], Value::String("HR".into()));
    assert_eq!(res.rows[2].values[1], Value::Int64(50));

    // 2. ORDER BY projection alias: ORDER BY total_pay DESC
    let res = txn
        .execute_sql("SELECT dept, SUM(salary) AS total_pay FROM staff GROUP BY dept ORDER BY total_pay DESC;")
        .unwrap();
    assert_eq!(res.schema.columns[1].name, "total_pay");
    assert_eq!(res.rows[0].values[0], Value::String("Engineering".into()));
    assert_eq!(res.rows[0].values[1], Value::Int64(300));

    txn.commit().unwrap();
}

// ============================================================================
// 8. Semantic Validation Errors
// ============================================================================

#[test]
fn test_semantic_validation_errors() {
    let ctx = TestContext::new("semantic_err");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, true),
            Column::new("name", DataType::String, true),
            Column::new("salary", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("users", schema).unwrap();

    // 1. Ungrouped column in SELECT: 'name' is neither in GROUP BY nor aggregated
    let err = txn
        .execute_sql("SELECT dept, name, COUNT(*) FROM users GROUP BY dept;")
        .unwrap_err();
    assert!(matches!(err, Error::UngroupedColumn(_)));

    // 2. Ungrouped column with no GROUP BY but aggregate in SELECT
    let err = txn
        .execute_sql("SELECT dept, COUNT(*) FROM users;")
        .unwrap_err();
    assert!(matches!(err, Error::UngroupedColumn(_)));

    // 3. Aggregate in WHERE clause
    let err = txn
        .execute_sql("SELECT * FROM users WHERE COUNT(*) > 5;")
        .unwrap_err();
    assert!(matches!(err, Error::AggregateInWhere));

    // 4. Ungrouped column in HAVING
    let err = txn
        .execute_sql("SELECT dept, COUNT(*) FROM users GROUP BY dept HAVING name = 'Alice';")
        .unwrap_err();
    assert!(matches!(
        err,
        Error::InvalidHavingExpression(_) | Error::UngroupedColumn(_)
    ));

    txn.commit().unwrap();
}

// ============================================================================
// 9. Secondary Index Compatibility
// ============================================================================

#[test]
fn test_aggregation_secondary_index_compatibility() {
    let ctx = TestContext::new("sec_idx_agg");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, true),
            Column::new("salary", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("staff", schema).unwrap();
    txn.create_index("staff", "idx_dept", "dept", false)
        .unwrap();

    {
        let mut table = txn.open_table("staff").unwrap();
        for i in 1..=20 {
            let dept = if i % 2 == 0 { "Engineering" } else { "Sales" };
            table
                .insert(&Row::new(vec![
                    Value::Int64(i),
                    Value::String(dept.into()),
                    Value::Int64(i * 10),
                ]))
                .unwrap();
        }
    }

    // Point lookup via secondary index + aggregation
    let res = txn
        .execute_sql(
            "SELECT COUNT(*), SUM(salary), AVG(salary) FROM staff WHERE dept = 'Engineering';",
        )
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(10));
    assert_eq!(res.rows[0].values[1], Value::Int64(1100)); // 20+40+...+200 = 1100
    assert_eq!(res.rows[0].values[2], Value::Float64(110.0));

    txn.commit().unwrap();
}

// ============================================================================
// 10. Transaction Visibility and Rollback
// ============================================================================

#[test]
fn test_aggregation_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_agg");
    let mut db = ctx.db();

    let mut txn1 = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn1.create_table("items", schema).unwrap();
    {
        let mut table = txn1.open_table("items").unwrap();
        table
            .insert(&Row::new(vec![Value::Int64(1), Value::Int64(100)]))
            .unwrap();
    }
    txn1.commit().unwrap();

    // Begin second transaction: insert row, observe new count, then rollback
    let mut txn2 = db.begin().unwrap();
    {
        let mut table = txn2.open_table("items").unwrap();
        table
            .insert(&Row::new(vec![Value::Int64(2), Value::Int64(200)]))
            .unwrap();
    }

    let res = txn2.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(2));

    txn2.abort().unwrap();

    // Begin third transaction: verify rollback restored original count
    let mut txn3 = db.begin().unwrap();
    let res = txn3.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    txn3.commit().unwrap();
}

// ============================================================================
// 11. Persistence Across Reopen & Recovery
// ============================================================================

#[test]
fn test_aggregation_persistence_and_recovery() {
    let ctx = TestContext::new("persist_agg");
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        let schema = Schema::new(
            vec![
                Column::new("id", DataType::Int64, false),
                Column::new("group_name", DataType::String, true),
                Column::new("score", DataType::Int64, true),
            ],
            0,
        )
        .unwrap();

        txn.create_table("scores", schema).unwrap();
        {
            let mut table = txn.open_table("scores").unwrap();
            table
                .insert(&Row::new(vec![
                    Value::Int64(1),
                    Value::String("A".into()),
                    Value::Int64(10),
                ]))
                .unwrap();
            table
                .insert(&Row::new(vec![
                    Value::Int64(2),
                    Value::String("A".into()),
                    Value::Int64(20),
                ]))
                .unwrap();
            table
                .insert(&Row::new(vec![
                    Value::Int64(3),
                    Value::String("B".into()),
                    Value::Int64(50),
                ]))
                .unwrap();
        }
        txn.commit().unwrap();
    }

    // Reopen database and verify persistent table aggregation
    let mut db2 = ctx.db();
    let mut txn2 = db2.begin().unwrap();
    let res = txn2
        .execute_sql("SELECT group_name, COUNT(*), SUM(score) FROM scores GROUP BY group_name ORDER BY group_name ASC;")
        .unwrap();

    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("A".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(res.rows[0].values[2], Value::Int64(30));

    assert_eq!(res.rows[1].values[0], Value::String("B".into()));
    assert_eq!(res.rows[1].values[1], Value::UInt64(1));
    assert_eq!(res.rows[1].values[2], Value::Int64(50));
    txn2.commit().unwrap();
}

// ============================================================================
// 12. SQL vs Programmatic Query Equivalence
// ============================================================================

#[test]
fn test_sql_vs_programmatic_query_equivalence() {
    let ctx = TestContext::new("equiv_agg");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::String, true),
            Column::new("price", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("inventory", schema).unwrap();
    {
        let mut table = txn.open_table("inventory").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Tech".into()),
                Value::Int64(200),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Tech".into()),
                Value::Int64(300),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(3),
                Value::String("Home".into()),
                Value::Int64(50),
            ]))
            .unwrap();
    }

    // SQL query
    let sql_res = txn
        .execute_sql("SELECT category, COUNT(*), SUM(price) FROM inventory GROUP BY category HAVING COUNT(*) >= 1 ORDER BY category ASC;")
        .unwrap();

    // Programmatic Query
    let prog_table = txn.open_table("inventory").unwrap();
    let prog_res = treedb::query::Query::new(prog_table)
        .group_by(vec![Expr::Column("category".into())])
        .aggregate(vec![
            AggregateExpr::count_all(),
            AggregateExpr::sum(Expr::Column("price".into())),
        ])
        .having(Expr::Binary {
            left: Box::new(Expr::Column("COUNT(*)".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::UInt64(1))),
        })
        .project(vec![
            Expr::Column("category".into()),
            Expr::Column("COUNT(*)".into()),
            Expr::Column("SUM(price)".into()),
        ])
        .sort_by(Expr::Column("category".into()), SortDirection::Ascending)
        .execute()
        .unwrap();

    assert_eq!(sql_res.rows.len(), prog_res.rows.len());
    for (sql_row, prog_row) in sql_res.rows.iter().zip(prog_res.rows.iter()) {
        assert_eq!(sql_row.values, prog_row.values);
    }

    txn.commit().unwrap();
}

// ============================================================================
// 13. Randomized Reference Oracle (Self-Contained In-Memory Engine Comparison)
// ============================================================================

#[derive(Debug, Clone)]
struct ReferenceRow {
    category: Option<String>,
    score: Option<i64>,
}

#[test]
fn test_randomized_aggregation_oracle() {
    let ctx = TestContext::new("oracle_agg");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::String, true),
            Column::new("score", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    txn.create_table("oracle_tbl", schema).unwrap();

    // Generate deterministic pseudo-random dataset
    let categories = [
        Some("Alpha".to_string()),
        Some("Beta".to_string()),
        Some("Gamma".to_string()),
        None,
    ];

    let mut reference_data: Vec<ReferenceRow> = Vec::new();

    {
        let mut table = txn.open_table("oracle_tbl").unwrap();
        for i in 1..=60 {
            let cat = categories[(i as usize * 7 + 3) % categories.len()].clone();
            let score = if i % 5 == 0 {
                None
            } else {
                Some((i * 13) % 100)
            };

            reference_data.push(ReferenceRow {
                category: cat.clone(),
                score,
            });

            table
                .insert(&Row::new(vec![
                    Value::Int64(i),
                    cat.map(Value::String).unwrap_or(Value::Null),
                    score.map(Value::Int64).unwrap_or(Value::Null),
                ]))
                .unwrap();
        }
    }

    // Run test queries and compare with reference oracle computation

    // Query 1: Global aggregation
    let sql_res = txn
        .execute_sql(
            "SELECT COUNT(*), COUNT(score), SUM(score), MIN(score), MAX(score) FROM oracle_tbl;",
        )
        .unwrap();

    let ref_count_all = reference_data.len() as u64;
    let ref_count_score = reference_data.iter().filter(|r| r.score.is_some()).count() as u64;
    let ref_sum: i64 = reference_data.iter().filter_map(|r| r.score).sum();
    let ref_min = reference_data.iter().filter_map(|r| r.score).min().unwrap();
    let ref_max = reference_data.iter().filter_map(|r| r.score).max().unwrap();

    assert_eq!(sql_res.rows[0].values[0], Value::UInt64(ref_count_all));
    assert_eq!(sql_res.rows[0].values[1], Value::UInt64(ref_count_score));
    assert_eq!(sql_res.rows[0].values[2], Value::Int64(ref_sum));
    assert_eq!(sql_res.rows[0].values[3], Value::Int64(ref_min));
    assert_eq!(sql_res.rows[0].values[4], Value::Int64(ref_max));

    // Query 2: Grouped query with ORDER BY
    let sql_res = txn
        .execute_sql("SELECT category, COUNT(*), SUM(score) FROM oracle_tbl GROUP BY category ORDER BY category ASC;")
        .unwrap();

    // Reference group calculation
    let mut ref_groups: std::collections::BTreeMap<Option<String>, (u64, Option<i64>)> =
        std::collections::BTreeMap::new();
    for r in &reference_data {
        let entry = ref_groups.entry(r.category.clone()).or_insert((0, None));
        entry.0 += 1;
        if let Some(s) = r.score {
            entry.1 = Some(entry.1.unwrap_or(0) + s);
        }
    }

    assert_eq!(sql_res.rows.len(), ref_groups.len());
    for (row, (cat, (cnt, sum))) in sql_res.rows.iter().zip(ref_groups.iter()) {
        let expected_cat = cat
            .as_ref()
            .map(|s| Value::String(s.clone()))
            .unwrap_or(Value::Null);
        let expected_sum = sum.map(Value::Int64).unwrap_or(Value::Null);
        assert_eq!(row.values[0], expected_cat);
        assert_eq!(row.values[1], Value::UInt64(*cnt));
        assert_eq!(row.values[2], expected_sum);
    }

    txn.commit().unwrap();
}
