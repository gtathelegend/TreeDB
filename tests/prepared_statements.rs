use std::path::PathBuf;
use treedb::engine::types::{DataType, Value};
use treedb::engine::{StatementResult, TreeDb};
use treedb::error::Error;
use treedb::query::optimizer::AccessPath;

fn create_temp_db(name: &str) -> (TreeDb, PathBuf) {
    let mut path = std::env::temp_dir();
    let unique_name = format!(
        "treedb_prepared_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    path.push(unique_name);
    let _ = std::fs::remove_file(&path);
    let wal_path = PathBuf::from(format!("{}.wal", path.display()));
    let _ = std::fs::remove_file(&wal_path);

    let db = TreeDb::create(&path, 50).unwrap();
    (db, path)
}

fn cleanup_temp_db(path: PathBuf) {
    let _ = std::fs::remove_file(&path);
    let wal_path = PathBuf::from(format!("{}.wal", path.display()));
    let _ = std::fs::remove_file(&wal_path);
}

#[test]
fn test_basic_parameter_lifecycle_and_reusability() {
    let (mut db, path) = create_temp_db("basic_lifecycle");

    db.execute_statement(
        "CREATE TABLE users (id UINT64 PRIMARY KEY, name STRING, age INT64, active BOOLEAN);",
    )
    .unwrap();

    db.execute_statement("INSERT INTO users VALUES (1, 'Alice', 30, true);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (2, 'Bob', 25, false);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (3, 'Charlie', 35, true);")
        .unwrap();

    // 1. Single parameter
    let stmt = db
        .prepare_sql("SELECT name FROM users WHERE id = ?;")
        .unwrap();
    assert_eq!(stmt.parameter_count(), 1);
    assert_eq!(
        stmt.parameter(1).unwrap().inferred_type,
        Some(DataType::UInt64)
    );

    let res1 = db
        .execute_prepared_query(&stmt, &[Value::UInt64(1)])
        .unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[0], Value::String("Alice".to_string()));

    // Reusability
    let res2 = db
        .execute_prepared_query(&stmt, &[Value::UInt64(2)])
        .unwrap();
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("Bob".to_string()));

    let res3 = db
        .execute_prepared_query(&stmt, &[Value::UInt64(99)])
        .unwrap();
    assert_eq!(res3.rows.len(), 0);

    // 2. Multiple parameters
    let multi_stmt = db
        .prepare_sql("SELECT name FROM users WHERE age >= ? AND age <= ?;")
        .unwrap();
    assert_eq!(multi_stmt.parameter_count(), 2);
    assert_eq!(
        multi_stmt.parameter(1).unwrap().inferred_type,
        Some(DataType::Int64)
    );
    assert_eq!(
        multi_stmt.parameter(2).unwrap().inferred_type,
        Some(DataType::Int64)
    );

    let res_range = db
        .execute_prepared_query(&multi_stmt, &[Value::Int64(25), Value::Int64(30)])
        .unwrap();
    assert_eq!(res_range.rows.len(), 2);

    // 3. Zero parameters
    let zero_stmt = db.prepare_sql("SELECT COUNT(*) FROM users;").unwrap();
    assert_eq!(zero_stmt.parameter_count(), 0);
    let res_zero = db.execute_prepared_query(&zero_stmt, &[]).unwrap();
    assert_eq!(res_zero.rows[0].values[0], Value::UInt64(3));

    cleanup_temp_db(path);
}

#[test]
fn test_parameter_count_validation() {
    let (mut db, path) = create_temp_db("param_count");

    db.execute_statement("CREATE TABLE t (id UINT64 PRIMARY KEY, a INT64, b INT64);")
        .unwrap();
    let stmt = db
        .prepare_sql("SELECT * FROM t WHERE a = ? AND b = ?;")
        .unwrap();
    assert_eq!(stmt.parameter_count(), 2);

    // Too few
    let err_too_few = db.execute_prepared(&stmt, &[Value::Int64(10)]).unwrap_err();
    match err_too_few {
        Error::ParameterCountMismatch { expected, received } => {
            assert_eq!(expected, 2);
            assert_eq!(received, 1);
        }
        other => panic!("expected ParameterCountMismatch, got {other:?}"),
    }

    // Too many
    let err_too_many = db
        .execute_prepared(
            &stmt,
            &[Value::Int64(10), Value::Int64(20), Value::Int64(30)],
        )
        .unwrap_err();
    match err_too_many {
        Error::ParameterCountMismatch { expected, received } => {
            assert_eq!(expected, 2);
            assert_eq!(received, 3);
        }
        other => panic!("expected ParameterCountMismatch, got {other:?}"),
    }

    cleanup_temp_db(path);
}

#[test]
fn test_parameter_types_and_validation() {
    let (mut db, path) = create_temp_db("param_types");

    db.execute_statement(
        "CREATE TABLE all_types (
            u UINT64 PRIMARY KEY,
            i INT64,
            f FLOAT64,
            b BOOLEAN,
            s STRING,
            bin BYTES
        );",
    )
    .unwrap();

    let insert_stmt = db
        .prepare_sql("INSERT INTO all_types VALUES (?, ?, ?, ?, ?, ?);")
        .unwrap();
    assert_eq!(insert_stmt.parameter_count(), 6);
    assert_eq!(
        insert_stmt.parameter(1).unwrap().inferred_type,
        Some(DataType::UInt64)
    );
    assert_eq!(
        insert_stmt.parameter(2).unwrap().inferred_type,
        Some(DataType::Int64)
    );
    assert_eq!(
        insert_stmt.parameter(3).unwrap().inferred_type,
        Some(DataType::Float64)
    );
    assert_eq!(
        insert_stmt.parameter(4).unwrap().inferred_type,
        Some(DataType::Boolean)
    );
    assert_eq!(
        insert_stmt.parameter(5).unwrap().inferred_type,
        Some(DataType::String)
    );
    assert_eq!(
        insert_stmt.parameter(6).unwrap().inferred_type,
        Some(DataType::Bytes)
    );

    // Valid execution with all data types
    let res = db
        .execute_prepared(
            &insert_stmt,
            &[
                Value::UInt64(1),
                Value::Int64(-42),
                Value::Float64(12.34),
                Value::Boolean(true),
                Value::String("hello".to_string()),
                Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            ],
        )
        .unwrap();
    assert_eq!(res, StatementResult::Inserted { rows: 1 });

    // Type mismatch on prepare validation (String passed for UInt64)
    let err_type = db
        .execute_prepared(
            &insert_stmt,
            &[
                Value::String("invalid".to_string()),
                Value::Int64(-42),
                Value::Float64(12.34),
                Value::Boolean(true),
                Value::String("hello".to_string()),
                Value::Bytes(vec![0x01]),
            ],
        )
        .unwrap_err();

    match err_type {
        Error::ParameterTypeMismatch {
            param_index,
            expected,
            found,
        } => {
            assert_eq!(param_index, 1);
            assert_eq!(expected, "UInt64");
            assert_eq!(found, "String");
        }
        other => panic!("expected ParameterTypeMismatch, got {other:?}"),
    }

    // Ambiguous type (SELECT ? FROM all_types WHERE u = 1)
    let ambig_stmt = db
        .prepare_sql("SELECT ? FROM all_types WHERE u = 1;")
        .unwrap();
    assert_eq!(ambig_stmt.parameter_count(), 1);
    assert_eq!(ambig_stmt.parameter(1).unwrap().inferred_type, None);

    let ambig_res = db
        .execute_prepared_query(&ambig_stmt, &[Value::String("resolved".to_string())])
        .unwrap();
    assert_eq!(
        ambig_res.rows[0].values[0],
        Value::String("resolved".to_string())
    );

    cleanup_temp_db(path);
}

#[test]
fn test_null_parameters_and_3vl() {
    let (mut db, path) = create_temp_db("null_params");

    db.execute_statement("CREATE TABLE items (id UINT64 PRIMARY KEY, name STRING);")
        .unwrap();
    db.execute_statement("INSERT INTO items VALUES (1, 'apple');")
        .unwrap();
    db.execute_statement("INSERT INTO items VALUES (2, NULL);")
        .unwrap();

    // WHERE name = ? with Value::Null must NOT return rows with NULL under 3VL (NULL = NULL evaluates to UNKNOWN)
    let stmt = db
        .prepare_sql("SELECT * FROM items WHERE name = ?;")
        .unwrap();
    let res = db.execute_prepared_query(&stmt, &[Value::Null]).unwrap();
    assert_eq!(res.rows.len(), 0);

    // ? IS NULL / ? IS NOT NULL
    let is_null_stmt = db
        .prepare_sql("SELECT id FROM items WHERE ? IS NULL;")
        .unwrap();
    let res_null = db
        .execute_prepared_query(&is_null_stmt, &[Value::Null])
        .unwrap();
    assert_eq!(res_null.rows.len(), 2);

    let res_not_null = db
        .execute_prepared_query(&is_null_stmt, &[Value::Int64(10)])
        .unwrap();
    assert_eq!(res_not_null.rows.len(), 0);

    cleanup_temp_db(path);
}

#[test]
fn test_optimizer_point_lookup_and_range_scan_with_parameters() {
    let (mut db, path) = create_temp_db("opt_params");

    db.execute_statement("CREATE TABLE t (id UINT64 PRIMARY KEY, val STRING);")
        .unwrap();
    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'val{i}');"))
            .unwrap();
    }

    // 1. PrimaryKeyPointLookup
    let point_stmt = db.prepare_sql("SELECT * FROM t WHERE id = ?;").unwrap();
    let mut txn = db.begin_transaction().unwrap();
    let plan = txn
        .plan_select_with_params(
            match &point_stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(&[Value::UInt64(5)]),
        )
        .unwrap();

    assert!(
        matches!(
            plan.base_table.access_path,
            AccessPath::PrimaryPointLookup {
                key: Value::UInt64(5)
            }
        ),
        "expected PrimaryPointLookup, found {:?}",
        plan.base_table.access_path
    );
    txn.commit().unwrap();

    // 2. PrimaryKeyRangeScan
    let range_stmt = db
        .prepare_sql("SELECT * FROM t WHERE id >= ? AND id < ?;")
        .unwrap();
    let mut txn = db.begin_transaction().unwrap();
    let plan_range = txn
        .plan_select_with_params(
            match &range_stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(&[Value::UInt64(5), Value::UInt64(15)]),
        )
        .unwrap();

    assert!(
        matches!(
            plan_range.base_table.access_path,
            AccessPath::PrimaryRangeScan {
                start: Some(Value::UInt64(5)),
                end: Some(Value::UInt64(15))
            }
        ),
        "expected PrimaryRangeScan, found {:?}",
        plan_range.base_table.access_path
    );
    txn.commit().unwrap();

    // 3. SecondaryIndexScan
    db.create_index("t", "idx_val", "val", false).unwrap();
    let sec_stmt = db.prepare_sql("SELECT * FROM t WHERE val = ?;").unwrap();
    let mut txn = db.begin_transaction().unwrap();
    let plan_sec = txn
        .plan_select_with_params(
            match &sec_stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(&[Value::String("val7".to_string())]),
        )
        .unwrap();

    assert!(
        matches!(plan_sec.base_table.access_path, AccessPath::SecondaryIndexScan { ref index_name, .. } if index_name == "idx_val"),
        "expected SecondaryIndexScan, found {:?}",
        plan_sec.base_table.access_path
    );
    txn.commit().unwrap();

    cleanup_temp_db(path);
}

#[test]
fn test_parameterized_dml_operations() {
    let (mut db, path) = create_temp_db("dml_params");

    db.execute_statement(
        "CREATE TABLE accounts (id UINT64 PRIMARY KEY, owner STRING, balance INT64);",
    )
    .unwrap();
    db.create_index("accounts", "idx_owner", "owner", false)
        .unwrap();

    // Parameterized INSERT
    let insert_stmt = db
        .prepare_sql("INSERT INTO accounts VALUES (?, ?, ?);")
        .unwrap();
    db.execute_prepared(
        &insert_stmt,
        &[
            Value::UInt64(1),
            Value::String("Alice".to_string()),
            Value::Int64(1000),
        ],
    )
    .unwrap();
    db.execute_prepared(
        &insert_stmt,
        &[
            Value::UInt64(2),
            Value::String("Bob".to_string()),
            Value::Int64(500),
        ],
    )
    .unwrap();

    // Parameterized UPDATE
    let update_stmt = db
        .prepare_sql("UPDATE accounts SET balance = ? WHERE owner = ?;")
        .unwrap();
    let upd_res = db
        .execute_prepared(
            &update_stmt,
            &[Value::Int64(1200), Value::String("Alice".to_string())],
        )
        .unwrap();
    assert_eq!(upd_res, StatementResult::Updated { rows: 1 });

    let verify_alice = db
        .execute_sql("SELECT balance FROM accounts WHERE id = 1;")
        .unwrap();
    assert_eq!(verify_alice.rows[0].values[0], Value::Int64(1200));

    // Parameterized DELETE
    let delete_stmt = db
        .prepare_sql("DELETE FROM accounts WHERE id = ?;")
        .unwrap();
    let del_res = db
        .execute_prepared(&delete_stmt, &[Value::UInt64(2)])
        .unwrap();
    assert_eq!(del_res, StatementResult::Deleted { rows: 1 });

    let count_res = db.execute_sql("SELECT COUNT(*) FROM accounts;").unwrap();
    assert_eq!(count_res.rows[0].values[0], Value::UInt64(1));

    cleanup_temp_db(path);
}

#[test]
fn test_parameterized_queries_joins_aggregates_subqueries_ctes_window() {
    let (mut db, path) = create_temp_db("advanced_queries");

    db.execute_statement("CREATE TABLE departments (id UINT64 PRIMARY KEY, name STRING);")
        .unwrap();
    db.execute_statement("CREATE TABLE employees (id UINT64 PRIMARY KEY, dept_id UINT64, name STRING, salary INT64);").unwrap();

    db.execute_statement("INSERT INTO departments VALUES (1, 'Engineering');")
        .unwrap();
    db.execute_statement("INSERT INTO departments VALUES (2, 'Sales');")
        .unwrap();

    db.execute_statement("INSERT INTO employees VALUES (1, 1, 'Alice', 120000);")
        .unwrap();
    db.execute_statement("INSERT INTO employees VALUES (2, 1, 'Bob', 110000);")
        .unwrap();
    db.execute_statement("INSERT INTO employees VALUES (3, 2, 'Charlie', 95000);")
        .unwrap();
    db.execute_statement("INSERT INTO employees VALUES (4, 2, 'Dave', 80000);")
        .unwrap();

    // 1. Parameterized JOIN
    let join_stmt = db.prepare_sql(
        "SELECT e.name, d.name FROM employees e JOIN departments d ON e.dept_id = d.id WHERE e.salary >= ?;"
    ).unwrap();
    let join_res = db
        .execute_prepared_query(&join_stmt, &[Value::Int64(100000)])
        .unwrap();
    assert_eq!(join_res.rows.len(), 2);

    // 2. Parameterized Aggregate & HAVING
    let agg_stmt = db
        .prepare_sql(
            "SELECT dept_id, COUNT(*) FROM employees GROUP BY dept_id HAVING AVG(salary) >= ?;",
        )
        .unwrap();
    let agg_res = db
        .execute_prepared_query(&agg_stmt, &[Value::Float64(100000.0)])
        .unwrap();
    assert_eq!(agg_res.rows.len(), 1);
    assert_eq!(agg_res.rows[0].values[0], Value::UInt64(1)); // Engineering

    // 3. Parameterized Subquery
    let subquery_stmt = db.prepare_sql(
        "SELECT name FROM employees WHERE dept_id IN (SELECT id FROM departments WHERE name = ?);"
    ).unwrap();
    let sub_res = db
        .execute_prepared_query(&subquery_stmt, &[Value::String("Sales".to_string())])
        .unwrap();
    assert_eq!(sub_res.rows.len(), 2);

    // 4. Parameterized CTE
    let cte_stmt = db.prepare_sql(
        "WITH high_earners AS (SELECT * FROM employees WHERE salary > ?) SELECT name FROM high_earners ORDER BY salary DESC;"
    ).unwrap();
    let cte_res = db
        .execute_prepared_query(&cte_stmt, &[Value::Int64(100000)])
        .unwrap();
    assert_eq!(cte_res.rows.len(), 2);
    assert_eq!(
        cte_res.rows[0].values[0],
        Value::String("Alice".to_string())
    );

    // 5. Parameterized Recursive CTE
    let rec_stmt = db.prepare_sql(
        "WITH RECURSIVE cnt(x) AS (SELECT id FROM departments WHERE id = 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < ?) SELECT SUM(x) FROM cnt;"
    ).unwrap();
    let rec_res = db
        .execute_prepared_query(&rec_stmt, &[Value::UInt64(5)])
        .unwrap();
    assert_eq!(rec_res.rows[0].values[0], Value::UInt64(15)); // 1 + 2 + 3 + 4 + 5 = 15

    // 6. Parameterized Set Operations
    let set_stmt = db.prepare_sql(
        "SELECT name FROM employees WHERE salary >= ? UNION ALL SELECT name FROM employees WHERE salary <= ?;"
    ).unwrap();
    let set_res = db
        .execute_prepared_query(&set_stmt, &[Value::Int64(120000), Value::Int64(80000)])
        .unwrap();
    assert_eq!(set_res.rows.len(), 2);

    // 7. Parameterized Window Functions
    let window_stmt = db.prepare_sql(
        "SELECT name, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY salary DESC) FROM employees WHERE salary >= ?;"
    ).unwrap();
    let win_res = db
        .execute_prepared_query(&window_stmt, &[Value::Int64(90000)])
        .unwrap();
    assert_eq!(win_res.rows.len(), 3);

    cleanup_temp_db(path);
}

#[test]
fn test_transaction_semantics_and_rollback_visibility() {
    let (mut db, path) = create_temp_db("txn_semantics");

    db.execute_statement("CREATE TABLE logs (id UINT64 PRIMARY KEY, msg STRING);")
        .unwrap();
    let insert_stmt = db.prepare_sql("INSERT INTO logs VALUES (?, ?);").unwrap();
    let select_stmt = db
        .prepare_sql("SELECT msg FROM logs WHERE id = ?;")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_prepared(
        &insert_stmt,
        &[Value::UInt64(1), Value::String("first".to_string())],
    )
    .unwrap();

    // Uncommitted visibility within same transaction
    let res = txn
        .execute_prepared_query(&select_stmt, &[Value::UInt64(1)])
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("first".to_string()));

    txn.abort().unwrap();

    // After rollback, row must not exist
    let res_after_abort = db
        .execute_prepared_query(&select_stmt, &[Value::UInt64(1)])
        .unwrap();
    assert_eq!(res_after_abort.rows.len(), 0);

    cleanup_temp_db(path);
}

#[test]
fn test_explain_and_explain_analyze_with_parameters() {
    let (mut db, path) = create_temp_db("explain_metrics");

    db.execute_statement(
        "CREATE TABLE products (id UINT64 PRIMARY KEY, name STRING, price INT64);",
    )
    .unwrap();
    for i in 1..=50 {
        db.execute_statement(&format!(
            "INSERT INTO products VALUES ({i}, 'product_{i}', {});",
            i * 10
        ))
        .unwrap();
    }

    let stmt = db
        .prepare_sql("SELECT * FROM products WHERE id >= ? AND id <= ?;")
        .unwrap();

    // EXPLAIN with parameter values
    let explain_plan = db
        .explain_prepared(&stmt, &[Value::UInt64(10), Value::UInt64(20)])
        .unwrap();
    assert!(
        explain_plan.contains("PrimaryRangeScan"),
        "plan: {explain_plan}"
    );

    // EXPLAIN ANALYZE with parameter values & execution metrics
    let (res, metrics) = db
        .execute_prepared_with_metrics(&stmt, &[Value::UInt64(10), Value::UInt64(20)])
        .unwrap();
    assert_eq!(res.rows.len(), 11);
    assert_eq!(metrics.total_rows, 11);
    assert!(metrics.root_operator.is_some());

    let explain_analyze_str = db
        .explain_prepared_with_metrics(&stmt, &[Value::UInt64(10), Value::UInt64(20)])
        .unwrap();
    assert!(explain_analyze_str.contains("actual_rows: 11"));

    cleanup_temp_db(path);
}

#[test]
fn test_sql_injection_immunity_and_identifier_restrictions() {
    let (mut db, path) = create_temp_db("security_tests");

    db.execute_statement("CREATE TABLE secure_users (id UINT64 PRIMARY KEY, username STRING);")
        .unwrap();
    db.execute_statement("INSERT INTO secure_users VALUES (1, 'admin');")
        .unwrap();

    let injection_payload = "admin' OR '1'='1; DROP TABLE secure_users; --";

    // 1. Storing injection payload via prepared statement
    let insert_stmt = db
        .prepare_sql("INSERT INTO secure_users VALUES (?, ?);")
        .unwrap();
    db.execute_prepared(
        &insert_stmt,
        &[
            Value::UInt64(2),
            Value::String(injection_payload.to_string()),
        ],
    )
    .unwrap();

    // 2. Querying by parameter matches verbatim string, does not execute payload or drop table
    let query_stmt = db
        .prepare_sql("SELECT * FROM secure_users WHERE username = ?;")
        .unwrap();
    let query_res = db
        .execute_prepared_query(&query_stmt, &[Value::String(injection_payload.to_string())])
        .unwrap();
    assert_eq!(query_res.rows.len(), 1);
    assert_eq!(query_res.rows[0].values[0], Value::UInt64(2));

    // Table still exists and has 2 rows
    let count_res = db
        .execute_sql("SELECT COUNT(*) FROM secure_users;")
        .unwrap();
    assert_eq!(count_res.rows[0].values[0], Value::UInt64(2));

    // 3. Rejection of parameters in DDL / identifier positions
    assert!(db
        .prepare_sql("CREATE TABLE ? (id UINT64 PRIMARY KEY);")
        .is_err());
    assert!(db.prepare_sql("SELECT * FROM ?;").is_err());
    assert!(db.prepare_sql("DROP TABLE ?;").is_err());

    cleanup_temp_db(path);
}

#[test]
fn test_prepared_statements_in_list_and_coercions() {
    let (mut db, path) = create_temp_db("in_list_coercion");

    db.execute_statement(
        "CREATE TABLE items (id UINT64 PRIMARY KEY, category STRING, price FLOAT64);",
    )
    .unwrap();
    db.execute_statement("INSERT INTO items VALUES (1, 'electronics', 199.99);")
        .unwrap();
    db.execute_statement("INSERT INTO items VALUES (2, 'books', 29.50);")
        .unwrap();
    db.execute_statement("INSERT INTO items VALUES (3, 'clothing', 49.00);")
        .unwrap();
    db.execute_statement("INSERT INTO items VALUES (4, 'groceries', 15.00);")
        .unwrap();

    // IN list with parameters
    let in_stmt = db
        .prepare_sql("SELECT id FROM items WHERE category IN (?, ?) ORDER BY id;")
        .unwrap();
    assert_eq!(in_stmt.parameter_count(), 2);
    assert_eq!(
        in_stmt.parameter(1).unwrap().inferred_type,
        Some(DataType::String)
    );
    assert_eq!(
        in_stmt.parameter(2).unwrap().inferred_type,
        Some(DataType::String)
    );

    let in_res = db
        .execute_prepared_query(
            &in_stmt,
            &[
                Value::String("books".to_string()),
                Value::String("clothing".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(in_res.rows.len(), 2);
    assert_eq!(in_res.rows[0].values[0], Value::UInt64(2));
    assert_eq!(in_res.rows[1].values[0], Value::UInt64(3));

    // Numeric coercion from Int64 to Float64 in parameter execution
    let price_stmt = db
        .prepare_sql("SELECT id FROM items WHERE price >= ?;")
        .unwrap();
    assert_eq!(
        price_stmt.parameter(1).unwrap().inferred_type,
        Some(DataType::Float64)
    );
    let price_res = db
        .execute_prepared_query(&price_stmt, &[Value::Int64(30)])
        .unwrap();
    assert_eq!(price_res.rows.len(), 2); // 199.99, 49.00

    cleanup_temp_db(path);
}

#[test]
fn test_prepared_dynamic_index_creation_adaptation() {
    let (mut db, path) = create_temp_db("dyn_index");

    db.execute_statement("CREATE TABLE metrics (id UINT64 PRIMARY KEY, device STRING, val INT64);")
        .unwrap();
    for i in 1..=100 {
        db.execute_statement(&format!(
            "INSERT INTO metrics VALUES ({i}, 'dev_{}', {i});",
            i % 10
        ))
        .unwrap();
    }

    let stmt = db
        .prepare_sql("SELECT * FROM metrics WHERE device = ?;")
        .unwrap();

    // Initially no secondary index on device -> FullTableScan
    let mut txn1 = db.begin_transaction().unwrap();
    let plan1 = txn1
        .plan_select_with_params(
            match &stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(&[Value::String("dev_3".to_string())]),
        )
        .unwrap();
    assert!(matches!(
        plan1.base_table.access_path,
        AccessPath::FullTableScan
    ));
    txn1.commit().unwrap();

    // Create index in a new transaction
    db.create_index("metrics", "idx_metrics_device", "device", false)
        .unwrap();

    // Next execution with same prepared statement now adapts to use the index (proving no stale physical plan is cached)
    let mut txn2 = db.begin_transaction().unwrap();
    let plan2 = txn2
        .plan_select_with_params(
            match &stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(&[Value::String("dev_3".to_string())]),
        )
        .unwrap();
    assert!(
        matches!(plan2.base_table.access_path, AccessPath::SecondaryIndexScan { ref index_name, .. } if index_name == "idx_metrics_device")
    );
    txn2.commit().unwrap();

    cleanup_temp_db(path);
}
