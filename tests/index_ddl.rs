use std::collections::BTreeMap;
use std::path::PathBuf;

use treedb::engine::types::{Row, Value};
use treedb::error::Error;
use treedb::sql::ast::{CreateIndexStatement, DropIndexStatement, Statement};
use treedb::{StatementResult, TreeDb};

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_index_ddl_{}_{}_{}.treedb",
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
// 1. Parser Tests: CREATE INDEX, CREATE UNIQUE INDEX, DROP INDEX
// ============================================================================

#[test]
fn test_parser_create_index_and_drop_index() {
    let queries = [
        (
            "CREATE INDEX idx_users_email ON users (email);",
            Statement::CreateIndex(CreateIndexStatement::new(
                "idx_users_email",
                "users",
                "email",
                false,
            )),
        ),
        (
            "create index idx_users_email on users (email)",
            Statement::CreateIndex(CreateIndexStatement::new(
                "idx_users_email",
                "users",
                "email",
                false,
            )),
        ),
        (
            "CREATE UNIQUE INDEX idx_uniq_email ON users (email);",
            Statement::CreateIndex(CreateIndexStatement::new(
                "idx_uniq_email",
                "users",
                "email",
                true,
            )),
        ),
        (
            "CrEaTe UnIqUe InDeX idx_uniq_email On users (email)",
            Statement::CreateIndex(CreateIndexStatement::new(
                "idx_uniq_email",
                "users",
                "email",
                true,
            )),
        ),
        (
            "DROP INDEX idx_users_email;",
            Statement::DropIndex(DropIndexStatement::new("idx_users_email")),
        ),
        (
            "drop index idx_users_email",
            Statement::DropIndex(DropIndexStatement::new("idx_users_email")),
        ),
        (
            "DrOp InDeX idx_users_email",
            Statement::DropIndex(DropIndexStatement::new("idx_users_email")),
        ),
    ];

    for (sql, expected) in queries {
        let stmt = treedb::sql::parse_statement(sql).unwrap();
        assert_eq!(stmt, expected, "failed parsing query: {sql}");
    }
}

#[test]
fn test_parser_index_ddl_malformed_syntax() {
    let invalid_queries = [
        "CREATE INDEX idx ON users;",         // Missing parentheses and column
        "CREATE INDEX idx ON users ();",      // Empty column list
        "CREATE INDEX idx ON users (a, b);",  // Multiple columns (composite) rejected
        "CREATE INDEX idx ON users (a + 1);", // Expression rejected
        "CREATE INDEX idx ON users (LOWER(a));", // Function call rejected
        "CREATE INDEX ON users (a);",         // Missing index name
        "CREATE INDEX idx users (a);",        // Missing ON
        "CREATE INDEX idx ON (a);",           // Missing table name
        "CREATE INDEX idx ON users (a) extra;", // Trailing garbage
        "CREATE UNIQUE users (a);",           // Missing INDEX
        "DROP INDEX;",                        // Missing index name
        "DROP INDEX idx extra;",              // Trailing garbage
        "DROP INDEX ON users (a);",           // Invalid DROP INDEX syntax
    ];

    for sql in invalid_queries {
        let res = treedb::sql::parse_statement(sql);
        assert!(res.is_err(), "expected error for malformed SQL: {sql}");
    }
}

// ============================================================================
// 2. Binder Tests
// ============================================================================

#[test]
fn test_binder_validations() {
    let ctx = TestContext::new("binder");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, name TEXT, age INT);")
        .unwrap();

    // 1. Unknown table
    let err = txn
        .execute_statement("CREATE INDEX idx ON nonexistent (name);")
        .unwrap_err();
    assert!(
        matches!(err, Error::TableNotFound(ref t) if t == "nonexistent"),
        "expected TableNotFound, got: {err:?}"
    );

    // 2. Unknown column
    let err = txn
        .execute_statement("CREATE INDEX idx ON t1 (nonexistent_col);")
        .unwrap_err();
    assert!(
        matches!(err, Error::IndexColumnNotFound(ref c) if c == "nonexistent_col"),
        "expected IndexColumnNotFound, got: {err:?}"
    );

    // 3. Valid creation
    txn.execute_statement("CREATE INDEX idx_name ON t1 (name);")
        .unwrap();

    // 4. Duplicate index name
    let err = txn
        .execute_statement("CREATE INDEX idx_name ON t1 (age);")
        .unwrap_err();
    assert!(
        matches!(err, Error::DuplicateIndexName(ref n) if n == "idx_name"),
        "expected DuplicateIndexName, got: {err:?}"
    );

    // 5. Invalid empty index name
    let t1_schema = txn.open_table("t1").unwrap().schema().clone();
    let bindings = [treedb::sql::TableBinding::new("t1", None, &t1_schema)];
    let err = treedb::sql::bind_create_index(
        &CreateIndexStatement::new("", "t1", "name", false),
        &(&bindings[..]),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidIndexName(_)));

    // 6. Drop nonexistent index
    let err = txn
        .execute_statement("DROP INDEX nonexistent_idx;")
        .unwrap_err();
    assert!(
        matches!(err, Error::IndexNotFound(ref n) if n == "nonexistent_idx"),
        "expected IndexNotFound, got: {err:?}"
    );
}

// ============================================================================
// 3. CREATE INDEX on Empty and Populated Tables
// ============================================================================

#[test]
fn test_create_index_on_empty_table() {
    let ctx = TestContext::new("empty");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, username TEXT, score INT);")
        .unwrap();

    let res = txn
        .execute_statement("CREATE INDEX idx_users_score ON users (score);")
        .unwrap();
    assert_eq!(
        res,
        StatementResult::CreatedIndex {
            index: "idx_users_score".into()
        }
    );

    let res_uniq = txn
        .execute_statement("CREATE UNIQUE INDEX idx_users_username ON users (username);")
        .unwrap();
    assert_eq!(
        res_uniq,
        StatementResult::CreatedIndex {
            index: "idx_users_username".into()
        }
    );

    // Validate structural integrity of created indexes
    txn.validate_index("idx_users_score").unwrap();
    txn.validate_index("idx_users_username").unwrap();
    txn.commit().unwrap();
}

#[test]
fn test_create_index_on_populated_table() {
    let ctx = TestContext::new("populated");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, tag TEXT, price INT);")
        .unwrap();

    // Insert 50 rows
    for i in 1..=50 {
        let sql = format!(
            "INSERT INTO items VALUES ({i}, 'tag_{}', {});",
            i % 5,
            i * 10
        );
        txn.execute_statement(&sql).unwrap();
    }

    // CREATE INDEX on pre-existing rows
    txn.execute_statement("CREATE INDEX idx_items_price ON items (price);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_items_tag ON items (tag);")
        .unwrap();

    // Query through index
    let res = txn
        .execute_sql("SELECT id, tag, price FROM items WHERE price = 250;")
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(
        res.rows[0],
        Row::new(vec![
            Value::Int64(25),
            Value::String("tag_0".into()),
            Value::Int64(250)
        ])
    );

    let res_tag = txn
        .execute_sql("SELECT id, tag, price FROM items WHERE tag = 'tag_2' ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res_tag.len(), 10);
    assert_eq!(res_tag.rows[0].values[0], Value::Int64(2));
    assert_eq!(res_tag.rows[9].values[0], Value::Int64(47));

    txn.commit().unwrap();
}

// ============================================================================
// 4. UNIQUE Index Semantics and NULL Handling
// ============================================================================

#[test]
fn test_unique_index_semantics_and_null_rule() {
    let ctx = TestContext::new("unique_null");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE accounts (id BIGINT PRIMARY KEY, email TEXT);")
        .unwrap();

    txn.execute_statement("CREATE UNIQUE INDEX idx_uniq_email ON accounts (email);")
        .unwrap();

    // 1. Insert unique values -> succeeds
    txn.execute_statement("INSERT INTO accounts VALUES (1, 'a@example.com');")
        .unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (2, 'b@example.com');")
        .unwrap();

    // 2. Insert duplicate non-null value -> fails
    let err = txn
        .execute_statement("INSERT INTO accounts VALUES (3, 'a@example.com');")
        .unwrap_err();
    assert!(
        matches!(err, Error::DuplicateIndexKey(_)),
        "expected DuplicateIndexKey, got: {err:?}"
    );

    // 3. First NULL value -> succeeds
    txn.execute_statement("INSERT INTO accounts VALUES (4, NULL);")
        .unwrap();

    // 4. Second NULL value -> fails with DuplicateIndexKey (TreeDB treats NULL as a single unique key)
    let err = txn
        .execute_statement("INSERT INTO accounts VALUES (5, NULL);")
        .unwrap_err();
    assert!(
        matches!(err, Error::DuplicateIndexKey(_)),
        "expected DuplicateIndexKey on second NULL, got: {err:?}"
    );

    txn.commit().unwrap();
}

#[test]
fn test_unique_index_creation_on_table_with_duplicates_fails() {
    let ctx = TestContext::new("uniq_dup_fail");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE emps (id BIGINT PRIMARY KEY, dept TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO emps VALUES (1, 'Engineering'), (2, 'Engineering');")
        .unwrap();

    // Creating unique index on table with existing duplicates must fail
    let err = txn
        .execute_statement("CREATE UNIQUE INDEX idx_dept ON emps (dept);")
        .unwrap_err();
    assert!(
        matches!(err, Error::DuplicateIndexKey(_)),
        "expected DuplicateIndexKey, got: {err:?}"
    );

    // Non-unique index succeeds
    txn.execute_statement("CREATE INDEX idx_dept ON emps (dept);")
        .unwrap();

    txn.commit().unwrap();
}

#[test]
fn test_non_unique_index_duplicates_and_multiple_nulls() {
    let ctx = TestContext::new("non_uniq_nulls");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE data (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_val ON data (val);")
        .unwrap();

    // Multiple duplicate values and multiple NULLs are permitted in non-unique index
    txn.execute_statement(
        "INSERT INTO data VALUES (1, 100), (2, 100), (3, 100), (4, NULL), (5, NULL);",
    )
    .unwrap();

    let res = txn
        .execute_sql("SELECT id, val FROM data WHERE val = 100 ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res.len(), 3);

    let res_null = txn
        .execute_sql("SELECT id, val FROM data WHERE val IS NULL ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res_null.len(), 2);

    txn.commit().unwrap();
}

// ============================================================================
// 5. Supported Data Types
// ============================================================================

#[test]
fn test_create_index_all_supported_data_types() {
    let ctx = TestContext::new("all_types");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    let sql_create = "CREATE TABLE all_types (
        id BIGINT PRIMARY KEY,
        b_val BOOLEAN,
        i_val BIGINT,
        u_val UBIGINT,
        f_val DOUBLE,
        s_val TEXT
    );";
    txn.execute_statement(sql_create).unwrap();

    // Create index on supported SQL literal data types
    txn.execute_statement("CREATE INDEX idx_bool ON all_types (b_val);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_int ON all_types (i_val);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_uint ON all_types (u_val);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_float ON all_types (f_val);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_str ON all_types (s_val);")
        .unwrap();

    // Insert sample data
    txn.execute_statement("INSERT INTO all_types VALUES (1, true, -500, 1000, 3.1415, 'hello');")
        .unwrap();
    txn.execute_statement("INSERT INTO all_types VALUES (2, false, 200, 5000, -2.718, 'world');")
        .unwrap();

    // Point queries for each index
    let r1 = txn
        .execute_sql("SELECT id FROM all_types WHERE b_val = true;")
        .unwrap();
    assert_eq!(r1.rows, vec![Row::new(vec![Value::Int64(1)])]);

    let r2 = txn
        .execute_sql("SELECT id FROM all_types WHERE i_val = -500;")
        .unwrap();
    assert_eq!(r2.rows, vec![Row::new(vec![Value::Int64(1)])]);

    let r3 = txn
        .execute_sql("SELECT id FROM all_types WHERE u_val = 5000;")
        .unwrap();
    assert_eq!(r3.rows, vec![Row::new(vec![Value::Int64(2)])]);

    let r4 = txn
        .execute_sql("SELECT id FROM all_types WHERE f_val = 3.1415;")
        .unwrap();
    assert_eq!(r4.rows, vec![Row::new(vec![Value::Int64(1)])]);

    let r5 = txn
        .execute_sql("SELECT id FROM all_types WHERE s_val = 'world';")
        .unwrap();
    assert_eq!(r5.rows, vec![Row::new(vec![Value::Int64(2)])]);

    // Also test BLOB column index creation
    txn.execute_statement("CREATE TABLE blobs (id BIGINT PRIMARY KEY, payload BLOB);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_blob ON blobs (payload);")
        .unwrap();
    txn.validate_index("idx_blob").unwrap();

    txn.commit().unwrap();
}

// ============================================================================
// 6. DROP INDEX Lifecycle
// ============================================================================

#[test]
fn test_drop_index_lifecycle() {
    let ctx = TestContext::new("drop_idx");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT, b INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_a ON t (a);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_b ON t (b);")
        .unwrap();

    txn.execute_statement("INSERT INTO t VALUES (1, 'alpha', 10), (2, 'beta', 20);")
        .unwrap();

    // Drop index `idx_a`
    let res = txn.execute_statement("DROP INDEX idx_a;").unwrap();
    assert_eq!(
        res,
        StatementResult::DroppedIndex {
            index: "idx_a".into()
        }
    );

    // `idx_a` is gone from catalog
    assert!(txn.get_index("idx_a").unwrap().is_none());

    // `idx_b` remains intact and valid
    assert!(txn.get_index("idx_b").unwrap().is_some());
    txn.validate_index("idx_b").unwrap();

    // Table rows are completely preserved
    let res_all = txn
        .execute_sql("SELECT id, a, b FROM t ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res_all.len(), 2);

    // Dropping again returns IndexNotFound
    let err = txn.execute_statement("DROP INDEX idx_a;").unwrap_err();
    assert!(matches!(err, Error::IndexNotFound(ref n) if n == "idx_a"));

    txn.commit().unwrap();
}

// ============================================================================
// 7. DML Mutations Maintain Indexes
// ============================================================================

#[test]
fn test_dml_mutations_maintain_index() {
    let ctx = TestContext::new("mutation_maint");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE products (id BIGINT PRIMARY KEY, sku TEXT, qty INT);")
        .unwrap();
    txn.execute_statement("CREATE UNIQUE INDEX idx_sku ON products (sku);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_qty ON products (qty);")
        .unwrap();

    // 1. INSERT maintains indexes
    txn.execute_statement("INSERT INTO products VALUES (1, 'SKU-001', 50), (2, 'SKU-002', 100);")
        .unwrap();

    let res1 = txn
        .execute_sql("SELECT id, qty FROM products WHERE sku = 'SKU-001';")
        .unwrap();
    assert_eq!(
        res1.rows,
        vec![Row::new(vec![Value::Int64(1), Value::Int64(50)])]
    );

    // 2. UPDATE maintains indexes
    txn.execute_statement("UPDATE products SET sku = 'SKU-001-MOD', qty = 75 WHERE id = 1;")
        .unwrap();

    // Old key no longer found
    let res_old = txn
        .execute_sql("SELECT id FROM products WHERE sku = 'SKU-001';")
        .unwrap();
    assert_eq!(res_old.len(), 0);

    // New key found
    let res_new = txn
        .execute_sql("SELECT id, qty FROM products WHERE sku = 'SKU-001-MOD';")
        .unwrap();
    assert_eq!(
        res_new.rows,
        vec![Row::new(vec![Value::Int64(1), Value::Int64(75)])]
    );

    // 3. DELETE maintains indexes
    txn.execute_statement("DELETE FROM products WHERE id = 2;")
        .unwrap();
    let res_del = txn
        .execute_sql("SELECT id FROM products WHERE sku = 'SKU-002';")
        .unwrap();
    assert_eq!(res_del.len(), 0);

    txn.validate_index("idx_sku").unwrap();
    txn.validate_index("idx_qty").unwrap();
    txn.commit().unwrap();
}

// ============================================================================
// 8. Optimizer and EXPLAIN Integration
// ============================================================================

#[test]
fn test_optimizer_access_path_and_explain() {
    let ctx = TestContext::new("opt_explain");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE customers (id BIGINT PRIMARY KEY, email TEXT, tier INT);")
        .unwrap();
    txn.execute_statement(
        "INSERT INTO customers VALUES (1, 'alice@corp.com', 1), (2, 'bob@corp.com', 2);",
    )
    .unwrap();

    // 1. Without index -> FullTableScan
    let plan_no_idx = txn
        .explain_sql("SELECT id, email FROM customers WHERE email = 'alice@corp.com';")
        .unwrap();
    assert!(
        plan_no_idx.contains("FullTableScan"),
        "expected FullTableScan, got:\n{plan_no_idx}"
    );

    // 2. CREATE INDEX -> SecondaryIndexScan
    txn.execute_statement("CREATE INDEX idx_cust_email ON customers (email);")
        .unwrap();

    let plan_with_idx = txn
        .explain_sql("SELECT id, email FROM customers WHERE email = 'alice@corp.com';")
        .unwrap();
    assert!(
        plan_with_idx
            .contains("SecondaryIndexScan(idx_cust_email, PointLookup(\"alice@corp.com\"))"),
        "expected SecondaryIndexScan, got:\n{plan_with_idx}"
    );

    // 3. DROP INDEX -> reverts to FullTableScan
    txn.execute_statement("DROP INDEX idx_cust_email;").unwrap();

    let plan_after_drop = txn
        .explain_sql("SELECT id, email FROM customers WHERE email = 'alice@corp.com';")
        .unwrap();
    assert!(
        plan_after_drop.contains("FullTableScan"),
        "expected FullTableScan after drop, got:\n{plan_after_drop}"
    );

    txn.commit().unwrap();
}

// ============================================================================
// 9. Transactions and Rollback
// ============================================================================

#[test]
fn test_transaction_create_index_rollback() {
    let ctx = TestContext::new("txn_create_rb");
    let mut db = ctx.db();

    // Setup table
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT);")
            .unwrap();
        txn.execute_statement("INSERT INTO t VALUES (1, 'val1'), (2, 'val2');")
            .unwrap();
        txn.commit().unwrap();
    }

    // Begin txn, CREATE INDEX, then ROLLBACK
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE INDEX idx_v ON t (v);")
            .unwrap();
        assert!(txn.get_index("idx_v").unwrap().is_some());
        txn.abort().unwrap();
    }

    // Verify index is absent after rollback
    {
        let mut txn = db.begin_transaction().unwrap();
        assert!(txn.get_index("idx_v").unwrap().is_none());
        let plan = txn
            .explain_sql("SELECT * FROM t WHERE v = 'val1';")
            .unwrap();
        assert!(plan.contains("FullTableScan"));
    }
}

#[test]
fn test_transaction_drop_index_rollback() {
    let ctx = TestContext::new("txn_drop_rb");
    let mut db = ctx.db();

    // Setup table and index
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT);")
            .unwrap();
        txn.execute_statement("CREATE INDEX idx_v ON t (v);")
            .unwrap();
        txn.execute_statement("INSERT INTO t VALUES (1, 'val1'), (2, 'val2');")
            .unwrap();
        txn.commit().unwrap();
    }

    // Begin txn, DROP INDEX, then ROLLBACK
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("DROP INDEX idx_v;").unwrap();
        assert!(txn.get_index("idx_v").unwrap().is_none());
        txn.abort().unwrap();
    }

    // Verify index is restored and functional after rollback
    {
        let mut txn = db.begin_transaction().unwrap();
        assert!(txn.get_index("idx_v").unwrap().is_some());
        txn.validate_index("idx_v").unwrap();
        let res = txn
            .execute_sql("SELECT id FROM t WHERE v = 'val1';")
            .unwrap();
        assert_eq!(res.rows, vec![Row::new(vec![Value::Int64(1)])]);
    }
}

// ============================================================================
// 10. Persistence Across Close and Reopen
// ============================================================================

#[test]
fn test_index_ddl_persistence_across_reopen() {
    let ctx = TestContext::new("persistence");

    // 1. Create table & index, insert data, commit
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, code TEXT, age INT);")
            .unwrap();
        txn.execute_statement("CREATE UNIQUE INDEX idx_code ON users (code);")
            .unwrap();
        txn.execute_statement("CREATE INDEX idx_age ON users (age);")
            .unwrap();
        txn.execute_statement("INSERT INTO users VALUES (1, 'C100', 25), (2, 'C200', 30);")
            .unwrap();
        txn.commit().unwrap();
    }

    // 2. Reopen database and verify indexes exist and operate correctly
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.validate_catalog().unwrap();
        txn.validate_index("idx_code").unwrap();
        txn.validate_index("idx_age").unwrap();

        let res = txn
            .execute_sql("SELECT id FROM users WHERE code = 'C200';")
            .unwrap();
        assert_eq!(res.rows, vec![Row::new(vec![Value::Int64(2)])]);

        // DROP one index, commit
        txn.execute_statement("DROP INDEX idx_code;").unwrap();
        txn.commit().unwrap();
    }

    // 3. Reopen again and verify dropped index is gone and remaining index is intact
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.validate_catalog().unwrap();
        assert!(txn.get_index("idx_code").unwrap().is_none());
        assert!(txn.get_index("idx_age").unwrap().is_some());
        txn.validate_index("idx_age").unwrap();

        let res_age = txn
            .execute_sql("SELECT id FROM users WHERE age = 25;")
            .unwrap();
        assert_eq!(res_age.rows, vec![Row::new(vec![Value::Int64(1)])]);
    }
}

// ============================================================================
// 11. Drop Table Cleans Up Indexes
// ============================================================================

#[test]
fn test_drop_table_cleans_up_indexes() {
    let ctx = TestContext::new("drop_table_cascade");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_a ON t (a);")
        .unwrap();
    assert!(txn.get_index("idx_a").unwrap().is_some());

    // DROP TABLE
    txn.execute_statement("DROP TABLE t;").unwrap();
    assert!(txn.get_index("idx_a").unwrap().is_none());
    txn.validate_catalog().unwrap();

    txn.commit().unwrap();
}

// ============================================================================
// 12. Randomized Reference Oracle Comparison
// ============================================================================

#[test]
fn test_randomized_index_ddl_oracle() {
    let ctx = TestContext::new("oracle");
    let mut db = ctx.db();

    // Setup table
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE oracle_t (id BIGINT PRIMARY KEY, val INT, tag TEXT);")
            .unwrap();
        txn.commit().unwrap();
    }

    // Reference state: id -> (val, tag)
    let mut ref_map: BTreeMap<i64, (Option<i64>, String)> = BTreeMap::new();
    let mut has_index_val = false;
    let mut has_index_tag = false;

    for step in 0..60i64 {
        let op_type = (step * 7 + 13) % 6;
        let mut txn = db.begin_transaction().unwrap();

        match op_type {
            0 | 1 => {
                // Insert
                let id = step + 1;
                let val = if (step % 5) == 0 {
                    None
                } else {
                    Some((step % 10) * 10)
                };
                let tag = format!("tag_{}", step % 7);

                let val_str = val.map(|v| v.to_string()).unwrap_or_else(|| "NULL".into());
                let sql = format!("INSERT INTO oracle_t VALUES ({id}, {val_str}, '{tag}');");
                txn.execute_statement(&sql).unwrap();
                ref_map.insert(id, (val, tag));
            }
            2 => {
                // Update
                if let Some((&id, _)) = ref_map.iter().next() {
                    let new_val_raw = (step * 3) % 100;
                    let new_val = Some(new_val_raw);
                    let new_tag = format!("updated_{step}");
                    let sql = format!("UPDATE oracle_t SET val = {new_val_raw}, tag = '{new_tag}' WHERE id = {id};");
                    txn.execute_statement(&sql).unwrap();
                    ref_map.insert(id, (new_val, new_tag));
                }
            }
            3 => {
                // Delete
                if let Some((&id, _)) = ref_map.iter().next_back() {
                    let sql = format!("DELETE FROM oracle_t WHERE id = {id};");
                    txn.execute_statement(&sql).unwrap();
                    ref_map.remove(&id);
                }
            }
            4 => {
                // Toggle Index on val
                if !has_index_val {
                    txn.execute_statement("CREATE INDEX idx_oracle_val ON oracle_t (val);")
                        .unwrap();
                    has_index_val = true;
                } else {
                    txn.execute_statement("DROP INDEX idx_oracle_val;").unwrap();
                    has_index_val = false;
                }
            }
            5 => {
                // Toggle Index on tag
                if !has_index_tag {
                    txn.execute_statement("CREATE INDEX idx_oracle_tag ON oracle_t (tag);")
                        .unwrap();
                    has_index_tag = true;
                } else {
                    txn.execute_statement("DROP INDEX idx_oracle_tag;").unwrap();
                    has_index_tag = false;
                }
            }
            _ => {}
        }

        // Query verification
        let target_tag = format!("tag_{}", step % 7);
        let sql = format!(
            "SELECT id, val, tag FROM oracle_t WHERE tag = '{target_tag}' ORDER BY id ASC;"
        );
        let res = txn.execute_sql(&sql).unwrap();

        let expected_rows: Vec<Row> = ref_map
            .iter()
            .filter(|(_, (_, tag))| tag == &target_tag)
            .map(|(&id, &(val, ref tag))| {
                Row::new(vec![
                    Value::Int64(id),
                    val.map(Value::Int64).unwrap_or(Value::Null),
                    Value::String(tag.clone()),
                ])
            })
            .collect();

        assert_eq!(
            res.rows, expected_rows,
            "step {step} mismatch between indexed query and reference oracle"
        );

        txn.commit().unwrap();
    }
}

// ============================================================================
// 13. Multiple Tables and Independent Indexes
// ============================================================================

#[test]
fn test_multiple_tables_independent_indexes() {
    let ctx = TestContext::new("multi_tables");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, code TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, code TEXT);")
        .unwrap();

    txn.execute_statement("CREATE INDEX idx_t1_code ON t1 (code);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_t2_code ON t2 (code);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 'C1');")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (1, 'C1');")
        .unwrap();

    // Drop idx_t1_code
    txn.execute_statement("DROP INDEX idx_t1_code;").unwrap();

    assert!(txn.get_index("idx_t1_code").unwrap().is_none());
    assert!(txn.get_index("idx_t2_code").unwrap().is_some());
    txn.validate_index("idx_t2_code").unwrap();

    let r2 = txn
        .execute_sql("SELECT id FROM t2 WHERE code = 'C1';")
        .unwrap();
    assert_eq!(r2.rows, vec![Row::new(vec![Value::Int64(1)])]);

    txn.commit().unwrap();
}

// ============================================================================
// 14. Crash Recovery around CREATE/DROP INDEX
// ============================================================================

#[test]
fn test_crash_recovery_index_ddl() {
    let ctx = TestContext::new("crash_recovery");

    // 1. Committed CREATE INDEX survives crash
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, sku TEXT, price INT);")
            .unwrap();
        txn.execute_statement("INSERT INTO items VALUES (1, 'SKU1', 100), (2, 'SKU2', 200);")
            .unwrap();
        txn.execute_statement("CREATE INDEX idx_sku ON items (sku);")
            .unwrap();
        txn.commit().unwrap();
    }

    // 2. Uncommitted DROP INDEX rolled back by recovery
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("DROP INDEX idx_sku;").unwrap();
        // Drop txn without committing (simulating crash)
    }

    // 3. Reopen and verify idx_sku is restored
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.validate_catalog().unwrap();
        assert!(txn.get_index("idx_sku").unwrap().is_some());
        txn.validate_index("idx_sku").unwrap();

        let res = txn
            .execute_sql("SELECT id FROM items WHERE sku = 'SKU2';")
            .unwrap();
        assert_eq!(res.rows, vec![Row::new(vec![Value::Int64(2)])]);
    }
}
