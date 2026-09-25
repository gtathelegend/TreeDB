use std::collections::BTreeMap;
use tempfile::tempdir;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::query::optimizer::AccessPath;
use treedb::{StatementResult, TreeDb};

// Helper to create a test database in a temporary directory
fn create_test_db() -> (TreeDb, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir)
}

fn reopen_db(dir: &tempfile::TempDir) -> TreeDb {
    let db_path = dir.path().join("test.db");
    TreeDb::open(&db_path, 64).unwrap()
}

// ============================================================================
// 1. DDL Tests: CREATE TABLE & DROP TABLE
// ============================================================================

#[test]
fn test_ddl_create_table_all_types_and_constraints() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    let sql = "CREATE TABLE users (
        id BIGINT PRIMARY KEY,
        name TEXT NOT NULL,
        score DOUBLE,
        count UBIGINT,
        active BOOLEAN,
        avatar BLOB
    );";

    let res = txn.execute_statement(sql).unwrap();
    assert_eq!(
        res,
        StatementResult::CreatedTable {
            table: "users".into()
        }
    );

    // Verify catalog schema
    let table = txn.open_table("users").unwrap();
    let schema = table.schema();
    assert_eq!(schema.primary_key_idx, 0);
    assert_eq!(schema.columns.len(), 6);
    assert_eq!(schema.columns[0].data_type, DataType::Int64);
    assert!(!schema.columns[0].nullable);
    assert_eq!(schema.columns[1].data_type, DataType::String);
    assert!(!schema.columns[1].nullable);
    assert_eq!(schema.columns[2].data_type, DataType::Float64);
    assert!(schema.columns[2].nullable);
    assert_eq!(schema.columns[3].data_type, DataType::UInt64);
    assert!(schema.columns[3].nullable);
    assert_eq!(schema.columns[4].data_type, DataType::Boolean);
    assert!(schema.columns[4].nullable);
    assert_eq!(schema.columns[5].data_type, DataType::Bytes);
    assert!(schema.columns[5].nullable);

    txn.commit().unwrap();
}

#[test]
fn test_ddl_create_table_constraint_pk_and_duplicate() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    let sql = "CREATE TABLE items (
        sku UBIGINT,
        title TEXT,
        PRIMARY KEY (sku)
    );";
    txn.execute_statement(sql).unwrap();

    // Duplicate table name error
    let err = txn.execute_statement(sql).unwrap_err();
    match err {
        Error::TableAlreadyExists(name) => assert_eq!(name, "items"),
        other => panic!("expected TableAlreadyExists, got {other:?}"),
    }

    txn.abort().unwrap();

    // After abort, table does not exist
    let mut txn2 = db.begin_transaction().unwrap();
    assert!(txn2.open_table("items").is_err());
}

#[test]
fn test_ddl_drop_table_success_and_errors() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (1, 'hello');")
        .unwrap();

    // Drop table
    let res = txn.execute_statement("DROP TABLE t1;").unwrap();
    assert_eq!(res, StatementResult::DroppedTable { table: "t1".into() });

    // Dropping again returns TableNotFound
    let err = txn.execute_statement("DROP TABLE t1;").unwrap_err();
    match err {
        Error::TableNotFound(name) => assert_eq!(name, "t1"),
        other => panic!("expected TableNotFound, got {other:?}"),
    }

    txn.commit().unwrap();
}

#[test]
fn test_ddl_rollback_and_persistence() {
    let (mut db, dir) = create_test_db();

    // 1. Rollback CREATE TABLE via abort
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE rolled_back (id BIGINT PRIMARY KEY);")
            .unwrap();
        txn.abort().unwrap();
    }

    // Verify it does not exist
    {
        let mut txn = db.begin_transaction().unwrap();
        assert!(txn.open_table("rolled_back").is_err());
    }

    // 2. Commit CREATE TABLE and reopen
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE persisted (id BIGINT PRIMARY KEY, name TEXT);")
            .unwrap();
        txn.commit().unwrap();
    }

    drop(db);
    let mut db = reopen_db(&dir);

    // Verify table is still present
    {
        let mut txn = db.begin_transaction().unwrap();
        let table = txn.open_table("persisted").unwrap();
        assert_eq!(table.schema().columns.len(), 2);
    }

    // 3. Rollback DROP TABLE via abort
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("INSERT INTO persisted VALUES (10, 'Alpha');")
            .unwrap();
        txn.commit().unwrap();
    }
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("DROP TABLE persisted;").unwrap();
        txn.abort().unwrap();
    }

    // Verify table and data still present
    {
        let mut txn = db.begin_transaction().unwrap();
        let query_res = txn.execute_sql("SELECT * FROM persisted;").unwrap();
        assert_eq!(query_res.rows.len(), 1);
        assert_eq!(query_res.rows[0].values[0], Value::Int64(10));
    }
}

// ============================================================================
// 2. DML Tests: INSERT
// ============================================================================

#[test]
fn test_dml_insert_column_list_and_type_coercion() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            name TEXT,
            age UBIGINT,
            score DOUBLE,
            active BOOLEAN
        );",
    )
    .unwrap();

    // Insert with column list
    let res = txn
        .execute_statement(
            "INSERT INTO users (id, name, score, age, active) VALUES (1, 'Alice', 95.5, 30, true);",
        )
        .unwrap();
    assert_eq!(res, StatementResult::Inserted { rows: 1 });

    // Insert without column list (positional) with numeric literal coercions
    let res2 = txn
        .execute_statement("INSERT INTO users VALUES (2, 'Bob', 25, 80, false);")
        .unwrap();
    assert_eq!(res2, StatementResult::Inserted { rows: 1 });

    // Multi-row INSERT
    let res3 = txn
        .execute_statement("INSERT INTO users (id, name) VALUES (3, 'Charlie'), (4, 'David');")
        .unwrap();
    assert_eq!(res3, StatementResult::Inserted { rows: 2 });

    let q = txn
        .execute_sql("SELECT id, name, age, score, active FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(q.rows.len(), 4);
    assert_eq!(
        q.rows[0].values,
        vec![
            Value::Int64(1),
            Value::String("Alice".into()),
            Value::UInt64(30),
            Value::Float64(95.5),
            Value::Boolean(true)
        ]
    );
    assert_eq!(
        q.rows[1].values,
        vec![
            Value::Int64(2),
            Value::String("Bob".into()),
            Value::UInt64(25),
            Value::Float64(80.0),
            Value::Boolean(false)
        ]
    );
    assert_eq!(
        q.rows[2].values,
        vec![
            Value::Int64(3),
            Value::String("Charlie".into()),
            Value::Null,
            Value::Null,
            Value::Null
        ]
    );
    assert_eq!(
        q.rows[3].values,
        vec![
            Value::Int64(4),
            Value::String("David".into()),
            Value::Null,
            Value::Null,
            Value::Null
        ]
    );

    txn.commit().unwrap();
}

#[test]
fn test_dml_insert_validations_and_duplicate_pk() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'Alice');")
        .unwrap();

    // Duplicate PK error
    let err = txn
        .execute_statement("INSERT INTO users VALUES (1, 'Bob');")
        .unwrap_err();
    match err {
        Error::DuplicatePrimaryKey(_) => {}
        other => panic!("expected DuplicatePrimaryKey, got {other:?}"),
    }

    // NULL constraint violation
    let err = txn
        .execute_statement("INSERT INTO users VALUES (2, NULL);")
        .unwrap_err();
    match err {
        Error::NullConstraintViolation { column } => assert_eq!(column, "name"),
        other => panic!("expected NullConstraintViolation, got {other:?}"),
    }

    // Missing column count mismatch
    let err = txn
        .execute_statement("INSERT INTO users (id, name) VALUES (3);")
        .unwrap_err();
    assert!(matches!(err, Error::ColumnCountMismatch { .. }));

    // Nonexistent column name
    let err = txn
        .execute_statement("INSERT INTO users (id, nonexistent) VALUES (3, 'foo');")
        .unwrap_err();
    assert!(matches!(err, Error::ColumnNotFound(_)));

    txn.commit().unwrap();
}

#[test]
fn test_dml_insert_with_secondary_indexes_and_unique_constraint() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            age BIGINT
        );",
    )
    .unwrap();

    // Create unique secondary index on email and non-unique on age
    txn.create_index("users", "idx_email", "email", true)
        .unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'alice@example.com', 30);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (2, 'bob@example.com', 30);")
        .unwrap();

    // Unique index violation
    let err = txn
        .execute_statement("INSERT INTO users VALUES (3, 'alice@example.com', 25);")
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateIndexKey(_)));

    // Verify index lookup via SQL WHERE
    let q = txn
        .execute_sql("SELECT id, email FROM users WHERE email = 'alice@example.com';")
        .unwrap();
    assert_eq!(q.rows.len(), 1);
    assert_eq!(q.rows[0].values[0], Value::Int64(1));

    // Verify non-unique index scan
    let q2 = txn
        .execute_sql("SELECT id FROM users WHERE age = 30 ORDER BY id;")
        .unwrap();
    assert_eq!(q2.rows.len(), 2);
    assert_eq!(q2.rows[0].values[0], Value::Int64(1));
    assert_eq!(q2.rows[1].values[0], Value::Int64(2));

    txn.commit().unwrap();
}

// ============================================================================
// 3. DML Tests: UPDATE
// ============================================================================

#[test]
fn test_dml_update_single_row_multi_row_and_all_rows() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age BIGINT,
            active BOOLEAN
        );",
    )
    .unwrap();

    txn.execute_statement(
        "INSERT INTO users VALUES (1, 'Alice', 20, true), (2, 'Bob', 30, false), (3, 'Charlie', 40, true);",
    )
    .unwrap();

    // 1. Single row update by PK
    let res = txn
        .execute_statement("UPDATE users SET age = 21, active = false WHERE id = 1;")
        .unwrap();
    assert_eq!(res, StatementResult::Updated { rows: 1 });

    let q1 = txn
        .execute_sql("SELECT age, active FROM users WHERE id = 1;")
        .unwrap();
    assert_eq!(
        q1.rows[0].values,
        vec![Value::Int64(21), Value::Boolean(false)]
    );

    // 2. Multi-row update with WHERE filter
    let res2 = txn
        .execute_statement("UPDATE users SET age = 50 WHERE active = true;")
        .unwrap();
    assert_eq!(res2, StatementResult::Updated { rows: 1 }); // Charlie is the only one active

    let q2 = txn
        .execute_sql("SELECT id, age FROM users ORDER BY id;")
        .unwrap();
    assert_eq!(q2.rows[0].values[1], Value::Int64(21)); // Alice
    assert_eq!(q2.rows[1].values[1], Value::Int64(30)); // Bob
    assert_eq!(q2.rows[2].values[1], Value::Int64(50)); // Charlie (updated to 50)

    // 3. UPDATE without WHERE (all rows)
    let res3 = txn
        .execute_statement("UPDATE users SET active = true;")
        .unwrap();
    assert_eq!(res3, StatementResult::Updated { rows: 3 });

    let q3 = txn
        .execute_sql("SELECT count(id) FROM users WHERE active = true;")
        .unwrap();
    assert_eq!(q3.rows[0].values[0], Value::UInt64(3));

    txn.commit().unwrap();
}

#[test]
fn test_dml_update_pk_rejection_and_null_constraints() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age BIGINT
        );",
    )
    .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'Alice', 20);")
        .unwrap();

    // Updating PK column must be rejected
    let err = txn
        .execute_statement("UPDATE users SET id = 2 WHERE id = 1;")
        .unwrap_err();
    assert!(matches!(err, Error::InvalidPrimaryKey(_)));

    // Setting NOT NULL column to NULL must be rejected
    let err2 = txn
        .execute_statement("UPDATE users SET name = NULL WHERE id = 1;")
        .unwrap_err();
    assert!(matches!(err2, Error::NullConstraintViolation { .. }));

    // Type mismatch error
    let err3 = txn
        .execute_statement("UPDATE users SET age = 'twenty' WHERE id = 1;")
        .unwrap_err();
    assert!(matches!(err3, Error::TypeMismatch { .. }));

    txn.commit().unwrap();
}

#[test]
fn test_dml_update_secondary_indexes_and_unique_conflict() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            score BIGINT
        );",
    )
    .unwrap();

    txn.create_index("users", "idx_email", "email", true)
        .unwrap();
    txn.create_index("users", "idx_score", "score", false)
        .unwrap();

    txn.execute_statement(
        "INSERT INTO users VALUES (1, 'alice@example.com', 100), (2, 'bob@example.com', 200);",
    )
    .unwrap();

    // Unique index conflict on UPDATE
    let err = txn
        .execute_statement("UPDATE users SET email = 'bob@example.com' WHERE id = 1;")
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateIndexKey(_)));

    // Verify row 1 still has original email and index is consistent
    let q1 = txn
        .execute_sql("SELECT id FROM users WHERE email = 'alice@example.com';")
        .unwrap();
    assert_eq!(q1.rows.len(), 1);

    // Update email to a new unique value
    txn.execute_statement("UPDATE users SET email = 'alice_new@example.com' WHERE id = 1;")
        .unwrap();

    // Old index value is gone, new index value is present
    let q_old = txn
        .execute_sql("SELECT id FROM users WHERE email = 'alice@example.com';")
        .unwrap();
    assert_eq!(q_old.rows.len(), 0);

    let q_new = txn
        .execute_sql("SELECT id FROM users WHERE email = 'alice_new@example.com';")
        .unwrap();
    assert_eq!(q_new.rows.len(), 1);
    assert_eq!(q_new.rows[0].values[0], Value::Int64(1));

    txn.commit().unwrap();
}

// ============================================================================
// 4. DML Tests: DELETE
// ============================================================================

#[test]
fn test_dml_delete_single_multi_and_all() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE items (
            id BIGINT PRIMARY KEY,
            tag TEXT,
            qty BIGINT
        );",
    )
    .unwrap();

    txn.create_index("items", "idx_tag", "tag", false).unwrap();

    txn.execute_statement(
        "INSERT INTO items VALUES 
            (1, 'red', 10),
            (2, 'blue', 20),
            (3, 'red', 30),
            (4, 'green', 40);",
    )
    .unwrap();

    // 1. Delete single row by PK
    let res = txn
        .execute_statement("DELETE FROM items WHERE id = 2;")
        .unwrap();
    assert_eq!(res, StatementResult::Deleted { rows: 1 });

    let q1 = txn
        .execute_sql("SELECT id FROM items ORDER BY id;")
        .unwrap();
    assert_eq!(q1.rows.len(), 3);

    // 2. Delete multiple rows via secondary index WHERE
    let res2 = txn
        .execute_statement("DELETE FROM items WHERE tag = 'red';")
        .unwrap();
    assert_eq!(res2, StatementResult::Deleted { rows: 2 });

    let q2 = txn
        .execute_sql("SELECT id FROM items ORDER BY id;")
        .unwrap();
    assert_eq!(q2.rows.len(), 1);
    assert_eq!(q2.rows[0].values[0], Value::Int64(4)); // only green remains

    // Verify secondary index has no entries for 'red'
    let q_tag = txn
        .execute_sql("SELECT id FROM items WHERE tag = 'red';")
        .unwrap();
    assert_eq!(q_tag.rows.len(), 0);

    // 3. DELETE without WHERE (all remaining rows)
    let res3 = txn.execute_statement("DELETE FROM items;").unwrap();
    assert_eq!(res3, StatementResult::Deleted { rows: 1 });

    let q3 = txn.execute_sql("SELECT count(id) FROM items;").unwrap();
    assert_eq!(q3.rows[0].values[0], Value::UInt64(0));

    txn.commit().unwrap();
}

// ============================================================================
// 5. Optimizer Verification for UPDATE / DELETE
// ============================================================================

#[test]
fn test_optimizer_reuse_for_update_and_delete() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            age BIGINT,
            active BOOLEAN
        );",
    )
    .unwrap();

    txn.create_index("users", "idx_email", "email", true)
        .unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();

    // 1. UPDATE with PK point lookup
    let plan_update_pk = txn
        .plan_sql("UPDATE users SET active = false WHERE id = 42;")
        .unwrap();
    assert!(
        matches!(
            plan_update_pk.base_table.access_path,
            AccessPath::PrimaryPointLookup { .. }
        ),
        "Expected PrimaryPointLookup, got {:?}",
        plan_update_pk.base_table.access_path
    );

    // 2. DELETE with Secondary index point lookup
    let plan_del_email = txn
        .plan_sql("DELETE FROM users WHERE email = 'alice@example.com';")
        .unwrap();
    assert!(
        matches!(
            plan_del_email.base_table.access_path,
            AccessPath::SecondaryIndexScan { ref index_name, .. } if index_name == "idx_email"
        ),
        "Expected SecondaryIndexScan on idx_email, got {:?}",
        plan_del_email.base_table.access_path
    );

    // 3. UPDATE with Range scan + residual filter
    let plan_update_range = txn
        .plan_sql(
            "UPDATE users SET active = false WHERE age >= 20 AND age <= 30 AND active = true;",
        )
        .unwrap();
    assert!(
        matches!(
            plan_update_range.base_table.access_path,
            AccessPath::SecondaryIndexScan { ref index_name, .. } if index_name == "idx_age"
        ),
        "Expected SecondaryIndexScan on idx_age, got {:?}",
        plan_update_range.base_table.access_path
    );
    assert!(plan_update_range.base_table.residual_filter.is_some());
}

// ============================================================================
// 6. Transaction Semantics & Crash Recovery
// ============================================================================

#[test]
fn test_dml_transaction_rollback_and_persistence() {
    let (mut db, dir) = create_test_db();

    // 1. Setup table
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE accounts (
                id BIGINT PRIMARY KEY,
                holder TEXT NOT NULL,
                balance BIGINT
            );",
        )
        .unwrap();
        txn.execute_statement("INSERT INTO accounts VALUES (1, 'Alice', 1000), (2, 'Bob', 2000);")
            .unwrap();
        txn.commit().unwrap();
    }

    // 2. Uncommitted mutations rolled back
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("INSERT INTO accounts VALUES (3, 'Charlie', 3000);")
            .unwrap();
        txn.execute_statement("UPDATE accounts SET balance = 500 WHERE id = 1;")
            .unwrap();
        txn.execute_statement("DELETE FROM accounts WHERE id = 2;")
            .unwrap();

        // Check read-your-writes inside txn
        let q = txn
            .execute_sql("SELECT id, balance FROM accounts ORDER BY id;")
            .unwrap();
        assert_eq!(q.rows.len(), 2); // 1 and 3 (2 deleted)
        assert_eq!(q.rows[0].values[1], Value::Int64(500));

        txn.abort().unwrap();
    }

    // 3. Verify original state after abort
    {
        let mut txn = db.begin_transaction().unwrap();
        let q = txn
            .execute_sql("SELECT id, balance FROM accounts ORDER BY id;")
            .unwrap();
        assert_eq!(q.rows.len(), 2);
        assert_eq!(q.rows[0].values[1], Value::Int64(1000));
        assert_eq!(q.rows[1].values[1], Value::Int64(2000));
    }

    // 4. Committed mutations survive close and reopen
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("UPDATE accounts SET balance = 1500 WHERE id = 1;")
            .unwrap();
        txn.execute_statement("DELETE FROM accounts WHERE id = 2;")
            .unwrap();
        txn.execute_statement("INSERT INTO accounts VALUES (4, 'David', 4000);")
            .unwrap();
        txn.commit().unwrap();
    }

    drop(db);
    let mut db = reopen_db(&dir);

    // Verify recovery after reopen
    {
        let mut txn = db.begin_transaction().unwrap();
        let q = txn
            .execute_sql("SELECT id, holder, balance FROM accounts ORDER BY id;")
            .unwrap();
        assert_eq!(q.rows.len(), 2);
        assert_eq!(
            q.rows[0].values,
            vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Int64(1500)
            ]
        );
        assert_eq!(
            q.rows[1].values,
            vec![
                Value::Int64(4),
                Value::String("David".into()),
                Value::Int64(4000)
            ]
        );
    }
}

#[test]
fn test_dml_overflow_records_mutations_and_reclamation() {
    let (mut db, _dir) = create_test_db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE docs (
            id BIGINT PRIMARY KEY,
            content TEXT
        );",
    )
    .unwrap();

    let large_text = "A".repeat(10000);
    let insert_sql = format!("INSERT INTO docs VALUES (1, '{large_text}');");
    txn.execute_statement(&insert_sql).unwrap();

    let q = txn
        .execute_sql("SELECT id, content FROM docs WHERE id = 1;")
        .unwrap();
    assert_eq!(q.rows.len(), 1);
    assert_eq!(q.rows[0].values[1], Value::String(large_text.clone()));

    // Update with another large string
    let updated_text = "B".repeat(12000);
    let update_sql = format!("UPDATE docs SET content = '{updated_text}' WHERE id = 1;");
    txn.execute_statement(&update_sql).unwrap();

    let q2 = txn
        .execute_sql("SELECT id, content FROM docs WHERE id = 1;")
        .unwrap();
    assert_eq!(q2.rows.len(), 1);
    assert_eq!(q2.rows[0].values[1], Value::String(updated_text));

    // Delete overflow record
    txn.execute_statement("DELETE FROM docs WHERE id = 1;")
        .unwrap();

    let q3 = txn.execute_sql("SELECT * FROM docs;").unwrap();
    assert_eq!(q3.rows.len(), 0);

    txn.commit().unwrap();
}

// ============================================================================
// 7. SQL vs Programmatic Equivalence
// ============================================================================

#[test]
fn test_sql_vs_programmatic_mutation_equivalence() {
    let (mut db1, _dir1) = create_test_db();
    let (mut db2, _dir2) = create_test_db();

    // DB1: purely programmatic Table mutations
    {
        let mut txn = db1.begin_transaction().unwrap();
        let schema = Schema::new(
            vec![
                Column::new("id".to_string(), DataType::Int64, false),
                Column::new("name".to_string(), DataType::String, false),
                Column::new("score".to_string(), DataType::Int64, true),
            ],
            0,
        )
        .unwrap();
        txn.create_table("users", schema).unwrap();
        txn.create_index("users", "idx_score", "score", false)
            .unwrap();

        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Int64(100),
            ]))
            .unwrap();
        table
            .insert(&Row::new(vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::Int64(200),
            ]))
            .unwrap();
        table
            .update(&Row::new(vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Int64(150),
            ]))
            .unwrap();
        table.delete(&Value::Int64(2)).unwrap();
        txn.commit().unwrap();
    }

    // DB2: purely SQL DDL and DML
    {
        let mut txn = db2.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE users (
                id BIGINT PRIMARY KEY,
                name TEXT NOT NULL,
                score BIGINT
            );",
        )
        .unwrap();
        txn.create_index("users", "idx_score", "score", false)
            .unwrap();

        txn.execute_statement("INSERT INTO users VALUES (1, 'Alice', 100), (2, 'Bob', 200);")
            .unwrap();
        txn.execute_statement("UPDATE users SET score = 150 WHERE id = 1;")
            .unwrap();
        txn.execute_statement("DELETE FROM users WHERE id = 2;")
            .unwrap();
        txn.commit().unwrap();
    }

    // Compare results
    let mut txn1 = db1.begin_transaction().unwrap();
    let mut txn2 = db2.begin_transaction().unwrap();

    let q1 = txn1
        .execute_sql("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();
    let q2 = txn2
        .execute_sql("SELECT id, name, score FROM users ORDER BY id;")
        .unwrap();

    assert_eq!(q1.rows, q2.rows);

    // Compare index lookup
    let idx_q1 = txn1
        .execute_sql("SELECT id FROM users WHERE score = 150;")
        .unwrap();
    let idx_q2 = txn2
        .execute_sql("SELECT id FROM users WHERE score = 150;")
        .unwrap();
    assert_eq!(idx_q1.rows, idx_q2.rows);
}

// ============================================================================
// 8. Randomized DML Oracle
// ============================================================================

#[test]
fn test_randomized_dml_oracle() {
    let (mut db, _dir) = create_test_db();

    // Create table with SQL
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE oracle (
                id BIGINT PRIMARY KEY,
                val BIGINT,
                active BOOLEAN
            );",
        )
        .unwrap();
        txn.commit().unwrap();
    }

    // Reference model
    let mut committed_state: BTreeMap<i64, (Option<i64>, Option<bool>)> = BTreeMap::new();

    // Simple pseudo-random generator
    let mut seed: u64 = 0xDEADBEEF;
    let mut rand_u64 = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    for _round in 0..40 {
        let mut txn = db.begin_transaction().unwrap();
        let mut uncommitted_state = committed_state.clone();
        let num_ops = (rand_u64() % 8) + 1;

        for _ in 0..num_ops {
            let op = rand_u64() % 4;

            match op {
                0 => {
                    // INSERT
                    let id = (rand_u64() % 30) as i64;
                    let val = (rand_u64() % 100) as i64;
                    let active = (rand_u64() % 2) == 1;

                    let sql = format!(
                        "INSERT INTO oracle (id, val, active) VALUES ({id}, {val}, {active});"
                    );
                    let res = txn.execute_statement(&sql);

                    if let std::collections::btree_map::Entry::Vacant(e) =
                        uncommitted_state.entry(id)
                    {
                        assert!(res.is_ok(), "Expected insert success: {res:?}");
                        e.insert((Some(val), Some(active)));
                    } else {
                        assert!(res.is_err(), "Expected duplicate PK error for id {id}");
                    }
                }
                1 => {
                    // UPDATE
                    let id = (rand_u64() % 30) as i64;
                    let new_val = (rand_u64() % 100) as i64;

                    let sql = format!("UPDATE oracle SET val = {new_val} WHERE id = {id};");
                    let res = txn.execute_statement(&sql).unwrap();

                    if let Some(entry) = uncommitted_state.get_mut(&id) {
                        assert_eq!(res, StatementResult::Updated { rows: 1 });
                        entry.0 = Some(new_val);
                    } else {
                        assert_eq!(res, StatementResult::Updated { rows: 0 });
                    }
                }
                2 => {
                    // DELETE
                    let id = (rand_u64() % 30) as i64;

                    let sql = format!("DELETE FROM oracle WHERE id = {id};");
                    let res = txn.execute_statement(&sql).unwrap();

                    if uncommitted_state.remove(&id).is_some() {
                        assert_eq!(res, StatementResult::Deleted { rows: 1 });
                    } else {
                        assert_eq!(res, StatementResult::Deleted { rows: 0 });
                    }
                }
                3 => {
                    // SELECT and verify uncommitted state
                    let q = txn
                        .execute_sql("SELECT id, val, active FROM oracle ORDER BY id;")
                        .unwrap();

                    assert_eq!(q.rows.len(), uncommitted_state.len());
                    for (i, (&expected_id, &(expected_val, expected_act))) in
                        uncommitted_state.iter().enumerate()
                    {
                        assert_eq!(q.rows[i].values[0], Value::Int64(expected_id));
                        assert_eq!(
                            q.rows[i].values[1],
                            expected_val.map(Value::Int64).unwrap_or(Value::Null)
                        );
                        assert_eq!(
                            q.rows[i].values[2],
                            expected_act.map(Value::Boolean).unwrap_or(Value::Null)
                        );
                    }
                }
                _ => unreachable!(),
            }
        }

        let should_commit = (rand_u64() % 3) != 0; // 66% commit, 33% abort
        if should_commit {
            txn.commit().unwrap();
            committed_state = uncommitted_state;
        } else {
            txn.abort().unwrap();
        }
    }

    // Final verification
    let mut final_txn = db.begin_transaction().unwrap();
    let q = final_txn
        .execute_sql("SELECT id, val, active FROM oracle ORDER BY id;")
        .unwrap();
    assert_eq!(q.rows.len(), committed_state.len());
    for (i, (&expected_id, &(expected_val, expected_act))) in committed_state.iter().enumerate() {
        assert_eq!(q.rows[i].values[0], Value::Int64(expected_id));
        assert_eq!(
            q.rows[i].values[1],
            expected_val.map(Value::Int64).unwrap_or(Value::Null)
        );
        assert_eq!(
            q.rows[i].values[2],
            expected_act.map(Value::Boolean).unwrap_or(Value::Null)
        );
    }
}
