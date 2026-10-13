//! Phase 87 — Index-Only Scans & Covering Index Optimization Correctness Suite
//!
//! Validates:
//! 1. Eligible single-column covering-index lookup.
//! 2. Eligible covering-index range scan.
//! 3. Composite covering-index lookup.
//! 4. Composite prefix and range queries.
//! 5. Queries requiring a non-indexed column that must fall back to record retrieval.
//! 6. Residual predicates that require indexed and non-indexed columns.
//! 7. Expressions requiring values from multiple columns.
//! 8. Duplicate keys and exact row multiplicity.
//! 9. NULL values.
//! 10. Indexed-column updates.
//! 11. Included-column updates.
//! 12. Deletes and reinserts.
//! 13. Transaction commit and abort.
//! 14. Historical snapshot visibility.
//! 15. Vacuum while older snapshots remain active.
//! 16. Covering-index correctness after vacuum.
//! 17. Checkpoint and reopen.
//! 18. Corrupt index or invalid payload handling.
//! 19. Empty result sets.
//! 20. Large range scans across multiple leaf pages.

use tempfile::TempDir;
use treedb::engine::isolation::{IsolationContext, IsolationLevel};
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::engine::vacuum::VacuumConfig;
use treedb::error::Result;
use treedb::index::secondary::IndexKey;
use treedb::query::optimizer::SecondaryScanType;
use treedb::query::{OperatorKind, OperatorMetrics};
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

fn find_table_scan_metric(m: &OperatorMetrics) -> Option<&OperatorMetrics> {
    if matches!(m.kind, OperatorKind::TableScan { .. }) {
        return Some(m);
    }
    for child in &m.children {
        if let Some(found) = find_table_scan_metric(child) {
            return Some(found);
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Eligible single-column covering-index lookup
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_01_eligible_single_column_covering_index_lookup() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t1.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("employees", schema)?;
    txn.execute_statement("CREATE INDEX idx_emp_dept ON employees (dept) INCLUDE (name, salary);")?;

    txn.execute_statement("INSERT INTO employees VALUES (1, 'engineering', 'Alice', 120000);")?;
    txn.execute_statement("INSERT INTO employees VALUES (2, 'sales', 'Bob', 90000);")?;
    txn.execute_statement("INSERT INTO employees VALUES (3, 'engineering', 'Charlie', 110000);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT dept, name, salary FROM employees WHERE dept = 'engineering';",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);

    let names: Vec<String> = res
        .rows
        .iter()
        .map(|r| match &r.values[1] {
            Value::String(s) => s.clone(),
            _ => panic!("unexpected type"),
        })
        .collect();
    assert!(names.contains(&"Alice".to_string()));
    assert!(names.contains(&"Charlie".to_string()));
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Eligible covering-index range scan
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_02_eligible_covering_index_range_scan() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t2.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("score", DataType::Int64, false),
            Column::new("badge", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("scores", schema)?;
    txn.execute_statement("CREATE INDEX idx_score_badge ON scores (score) INCLUDE (badge);")?;

    for i in 1..=20 {
        txn.execute_statement(&format!(
            "INSERT INTO scores VALUES ({}, {}, 'badge_{}');",
            i,
            i * 10,
            i
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT score, badge FROM scores WHERE score >= 50 AND score <= 120 ORDER BY score;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 8); // 50, 60, 70, 80, 90, 100, 110, 120
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 8);

    assert_eq!(res.rows[0].values[0], Value::Int64(50));
    assert_eq!(res.rows[7].values[0], Value::Int64(120));
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Composite covering-index lookup
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_03_composite_covering_index_lookup() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t3.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("tenant", DataType::String, false),
            Column::new("user_id", DataType::Int64, false),
            Column::new("role", DataType::String, false),
            Column::new("extra", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("members", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_tenant_user ON members (tenant, user_id) INCLUDE (role);",
    )?;

    txn.execute_statement("INSERT INTO members VALUES (1, 'acme', 101, 'admin', 'extra_data_1');")?;
    txn.execute_statement(
        "INSERT INTO members VALUES (2, 'acme', 102, 'editor', 'extra_data_2');",
    )?;
    txn.execute_statement(
        "INSERT INTO members VALUES (3, 'globex', 101, 'viewer', 'extra_data_3');",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT tenant, user_id, role FROM members WHERE tenant = 'acme' AND user_id = 101;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[2], Value::String("admin".to_string()));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 1);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Composite prefix and range queries
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_04_composite_prefix_and_range_queries() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t4.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("rank", DataType::Int64, false),
            Column::new("title", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("ranks", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_rank ON ranks (dept, rank) INCLUDE (title);")?;

    txn.execute_statement("INSERT INTO ranks VALUES (1, 'rnd', 1, 'Junior');")?;
    txn.execute_statement("INSERT INTO ranks VALUES (2, 'rnd', 2, 'Senior');")?;
    txn.execute_statement("INSERT INTO ranks VALUES (3, 'rnd', 3, 'Principal');")?;
    txn.execute_statement("INSERT INTO ranks VALUES (4, 'qa', 1, 'Junior QA');")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT dept, rank, title FROM ranks WHERE dept = 'rnd' AND rank >= 2 ORDER BY rank;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::Int64(2));
    assert_eq!(res.rows[1].values[1], Value::Int64(3));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 3);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Queries requiring a non-indexed column fall back to record retrieval
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_05_queries_requiring_non_indexed_column_fall_back() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t5.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("notes", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    // idx_dept only covers dept and name, NOT notes!
    txn.execute_statement("CREATE INDEX idx_dept ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement("INSERT INTO staff VALUES (1, 'sales', 'Sam', 'top seller');")?;
    txn.execute_statement("INSERT INTO staff VALUES (2, 'sales', 'Sally', 'new hire');")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // Query requires notes, so cannot be index-only
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT dept, name, notes FROM staff WHERE dept = 'sales';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 2);
    assert_eq!(scan.counters.base_records_avoided, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Residual predicates requiring indexed and non-indexed columns
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_06_residual_predicates_requiring_non_indexed_columns() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t6.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("secret_code", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("agents", schema)?;
    txn.execute_statement("CREATE INDEX idx_agent_dept ON agents (dept) INCLUDE (name);")?;

    txn.execute_statement("INSERT INTO agents VALUES (1, 'mi6', 'James', 007);")?;
    txn.execute_statement("INSERT INTO agents VALUES (2, 'mi6', 'Eve', 123);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // Residual predicate references secret_code (not in index)
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT name FROM agents WHERE dept = 'mi6' AND secret_code = 7;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("James".to_string()));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 2);
    assert_eq!(scan.counters.base_records_avoided, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. Expressions requiring values from multiple columns
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_07_expressions_requiring_values_from_multiple_columns() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t7.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("cat", DataType::String, false),
            Column::new("base_fee", DataType::Int64, false),
            Column::new("tax", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("fees", schema)?;
    txn.execute_statement("CREATE INDEX idx_fee ON fees (cat) INCLUDE (base_fee, tax);")?;

    txn.execute_statement("INSERT INTO fees VALUES (1, 'tier1', 100, 15);")?;
    txn.execute_statement("INSERT INTO fees VALUES (2, 'tier1', 200, 30);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT cat, base_fee + tax FROM fees WHERE cat = 'tier1' ORDER BY base_fee;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::Int64(115));
    assert_eq!(res.rows[1].values[1], Value::Int64(230));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 8. Duplicate keys and exact row multiplicity
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_08_duplicate_keys_and_exact_row_multiplicity() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t8.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("status", DataType::String, false),
            Column::new("amount", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("orders", schema)?;
    txn.execute_statement("CREATE INDEX idx_status ON orders (status) INCLUDE (amount);")?;

    for i in 1..=50 {
        txn.execute_statement(&format!(
            "INSERT INTO orders VALUES ({}, 'pending', {});",
            i,
            i * 10
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT status, amount FROM orders WHERE status = 'pending';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 50);
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 50);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 9. NULL values handling
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_09_null_values_handling() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t9.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("code", DataType::String, true),
            Column::new("description", DataType::String, true),
        ],
        0,
    )?;
    txn.create_table("items", schema)?;
    txn.execute_statement("CREATE INDEX idx_item_code ON items (code) INCLUDE (description);")?;

    txn.execute_statement("INSERT INTO items VALUES (1, NULL, 'desc_null_code');")?;
    txn.execute_statement("INSERT INTO items VALUES (2, 'c1', NULL);")?;
    txn.execute_statement("INSERT INTO items VALUES (3, 'c2', 'desc_c2');")?;
    txn.commit()?;

    // Direct programmatic index-only scan on NULL key
    {
        let mut tbl = db.open_table("items")?;
        let rows_null = tbl.index_only_scan(
            "idx_item_code",
            &SecondaryScanType::PointLookup(Value::Null),
        )?;
        assert_eq!(rows_null.len(), 1);
        assert_eq!(rows_null[0].values[1], Value::Null);
        assert_eq!(
            rows_null[0].values[2],
            Value::String("desc_null_code".to_string())
        );
    }

    // Query non-null key with NULL included column
    let mut txn = db.begin_transaction()?;
    let (res2, metrics2) =
        txn.execute_sql_with_metrics("SELECT code, description FROM items WHERE code = 'c1';")?;
    txn.commit()?;

    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[0], Value::String("c1".to_string()));
    assert_eq!(res2.rows[0].values[1], Value::Null);
    let scan2 = find_table_scan_metric(metrics2.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan2.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 10. Indexed-column updates
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_10_indexed_column_updates() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t10.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("state", DataType::String, false),
            Column::new("info", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("tasks", schema)?;
    txn.execute_statement("CREATE INDEX idx_task_state ON tasks (state) INCLUDE (info);")?;

    txn.execute_statement("INSERT INTO tasks VALUES (1, 'ready', 'task1');")?;
    txn.commit()?;

    // Update indexed column
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE tasks SET state = 'running' WHERE id = 1;")?;
    txn.commit()?;

    // Old key returns 0 rows
    let mut txn = db.begin_transaction()?;
    let (res_old, _) =
        txn.execute_sql_with_metrics("SELECT state, info FROM tasks WHERE state = 'ready';")?;
    txn.commit()?;
    assert_eq!(res_old.rows.len(), 0);

    // New key returns updated row index-only
    let mut txn = db.begin_transaction()?;
    let (res_new, metrics) =
        txn.execute_sql_with_metrics("SELECT state, info FROM tasks WHERE state = 'running';")?;
    txn.commit()?;
    assert_eq!(res_new.rows.len(), 1);
    assert_eq!(
        res_new.rows[0].values[1],
        Value::String("task1".to_string())
    );
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 11. Included-column updates
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_11_included_column_updates() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t11.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("tag", DataType::String, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("props", schema)?;
    txn.execute_statement("CREATE INDEX idx_prop_tag ON props (tag) INCLUDE (val);")?;

    txn.execute_statement("INSERT INTO props VALUES (1, 'alpha', 100);")?;
    txn.commit()?;

    // Update included column only
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE props SET val = 250 WHERE id = 1;")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT tag, val FROM props WHERE tag = 'alpha';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::Int64(250));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 12. Deletes and reinserts
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_12_deletes_and_reinserts() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t12.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("sku", DataType::String, false),
            Column::new("stock", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("inventory", schema)?;
    txn.execute_statement("CREATE INDEX idx_inv_sku ON inventory (sku) INCLUDE (stock);")?;

    txn.execute_statement("INSERT INTO inventory VALUES (1, 'SKU-01', 50);")?;
    txn.commit()?;

    // Delete
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("DELETE FROM inventory WHERE id = 1;")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res_del, _) =
        txn.execute_sql_with_metrics("SELECT sku, stock FROM inventory WHERE sku = 'SKU-01';")?;
    txn.commit()?;
    assert_eq!(res_del.rows.len(), 0);

    // Reinsert
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("INSERT INTO inventory VALUES (2, 'SKU-01', 75);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res_reinsert, metrics) =
        txn.execute_sql_with_metrics("SELECT sku, stock FROM inventory WHERE sku = 'SKU-01';")?;
    txn.commit()?;
    assert_eq!(res_reinsert.rows.len(), 1);
    assert_eq!(res_reinsert.rows[0].values[1], Value::Int64(75));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 13. Transaction commit and abort
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_13_transaction_commit_and_abort() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t13.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("grp", DataType::String, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("txn_test", schema)?;
    txn.execute_statement("CREATE INDEX idx_grp ON txn_test (grp) INCLUDE (val);")?;
    txn.execute_statement("INSERT INTO txn_test VALUES (1, 'g1', 10);")?;
    txn.commit()?;

    // Uncommitted abort
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("INSERT INTO txn_test VALUES (2, 'g1', 20);")?;
    txn.abort()?;

    // Only committed row visible
    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT grp, val FROM txn_test WHERE grp = 'g1';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::Int64(10));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 14. Historical snapshot visibility
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_14_historical_snapshot_visibility() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t14.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("acc", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("accounts", schema)?;
    txn.execute_statement("CREATE INDEX idx_acc ON accounts (acc) INCLUDE (balance);")?;
    txn.execute_statement("INSERT INTO accounts VALUES (1, 'A100', 500);")?;
    txn.commit()?;

    // Register snapshot at this point
    let snap = db.register_snapshot();

    // Concurrent updater modifies balance to 999 and commits
    let mut updater_txn = db.begin_transaction()?;
    updater_txn.execute_statement("UPDATE accounts SET balance = 999 WHERE id = 1;")?;
    updater_txn.commit()?;

    // Historical snapshot still sees original 500!
    {
        let mut tbl = db.open_table("accounts")?;
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(999),
            snap,
        )));
        let rows_snap = tbl.index_only_scan(
            "idx_acc",
            &SecondaryScanType::PointLookup(Value::String("A100".into())),
        )?;
        assert_eq!(rows_snap.len(), 1);
        assert_eq!(rows_snap[0].values[2], Value::Int64(500));
    }

    db.release_snapshot(snap.snapshot_id);

    // Subsequent transaction sees 999
    let mut current_txn = db.begin_transaction()?;
    let (res_curr, metrics) = current_txn
        .execute_sql_with_metrics("SELECT acc, balance FROM accounts WHERE acc = 'A100';")?;
    current_txn.commit()?;

    assert_eq!(res_curr.rows.len(), 1);
    assert_eq!(res_curr.rows[0].values[1], Value::Int64(999));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 15. Vacuum while older snapshots remain active
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_15_vacuum_while_older_snapshots_remain_active() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t15.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("topic", DataType::String, false),
            Column::new("author", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("posts", schema)?;
    txn.execute_statement("CREATE INDEX idx_post_topic ON posts (topic) INCLUDE (author);")?;
    txn.execute_statement("INSERT INTO posts VALUES (1, 'rust', 'Carol');")?;
    txn.commit()?;

    // Active snapshot
    let snap = db.register_snapshot();

    // Another txn deletes post 1
    let mut del_txn = db.begin_transaction()?;
    del_txn.execute_statement("DELETE FROM posts WHERE id = 1;")?;
    del_txn.commit()?;

    // Vacuum should not remove version required by snap
    db.vacuum(&VacuumConfig::unlimited())?;

    // Active snapshot still sees Carol
    {
        let mut tbl = db.open_table("posts")?;
        tbl.set_isolation_context(Some(IsolationContext::new(
            IsolationLevel::RepeatableRead,
            TxnId(999),
            snap,
        )));
        let rows = tbl.index_only_scan(
            "idx_post_topic",
            &SecondaryScanType::PointLookup(Value::String("rust".into())),
        )?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[2], Value::String("Carol".into()));
    }

    db.release_snapshot(snap.snapshot_id);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 16. Covering-index correctness after vacuum
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_16_covering_index_correctness_after_vacuum() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t16.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("loc", DataType::String, false),
            Column::new("temp", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("weather", schema)?;
    txn.execute_statement("CREATE INDEX idx_weather_loc ON weather (loc) INCLUDE (temp);")?;

    txn.execute_statement("INSERT INTO weather VALUES (1, 'north', 10);")?;
    txn.execute_statement("INSERT INTO weather VALUES (2, 'south', 30);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("DELETE FROM weather WHERE id = 1;")?;
    txn.commit()?;

    // Vacuum dead rows
    db.vacuum(&VacuumConfig::unlimited())?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT loc, temp FROM weather WHERE loc = 'south';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::Int64(30));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 17. Checkpoint and reopen
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_17_checkpoint_and_reopen() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t17.treedb");

    {
        let mut db = TreeDb::create(&db_path, 64)?;
        let mut txn = db.begin_transaction()?;
        let schema = Schema::new(
            vec![
                Column::new("id", DataType::UInt64, false),
                Column::new("client", DataType::String, false),
                Column::new("tier", DataType::String, false),
            ],
            0,
        )?;
        txn.create_table("clients", schema)?;
        txn.execute_statement("CREATE INDEX idx_client_tier ON clients (client) INCLUDE (tier);")?;
        txn.execute_statement("INSERT INTO clients VALUES (1, 'client_A', 'gold');")?;
        txn.execute_statement("INSERT INTO clients VALUES (2, 'client_B', 'silver');")?;
        txn.commit()?;
        db.checkpoint()?;
    }

    // Reopen DB
    let mut db = TreeDb::open(&db_path, 64)?;
    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn
        .execute_sql_with_metrics("SELECT client, tier FROM clients WHERE client = 'client_A';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("gold".into()));
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 1);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 18. Corrupt index or invalid payload handling
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_18_corrupt_index_or_invalid_payload_handling() {
    // Verify IndexKey::decode_bytes safely rejects malformed or truncated payloads
    let truncated_bytes = [1u8]; // Truncated length prefix for String
    let decoded = IndexKey::decode_bytes(&truncated_bytes, DataType::String);
    assert!(decoded.is_err(), "Truncated bytes must return Error");

    let invalid_bool_byte = [5u8]; // Invalid boolean value
    let decoded_bool = IndexKey::decode_bytes(&invalid_bool_byte, DataType::Boolean);
    assert!(
        decoded_bool.is_err(),
        "Invalid boolean byte must return Error"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 19. Empty result sets
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_19_empty_result_sets() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t19.treedb");
    let mut db = TreeDb::create(&db_path, 64)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("brand", DataType::String, false),
            Column::new("rating", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("brands", schema)?;
    txn.execute_statement("CREATE INDEX idx_brand ON brands (brand) INCLUDE (rating);")?;
    txn.execute_statement("INSERT INTO brands VALUES (1, 'acme', 5);")?;
    txn.commit()?;

    // Missing key lookup
    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT brand, rating FROM brands WHERE brand = 'nonexistent';",
    )?;
    txn.commit()?;
    assert_eq!(res.rows.len(), 0);
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);

    // Range scan matching 0 rows
    let mut txn = db.begin_transaction()?;
    let (res2, _) = txn.execute_sql_with_metrics(
        "SELECT brand, rating FROM brands WHERE brand >= 'zoo' AND brand <= 'zzz';",
    )?;
    txn.commit()?;
    assert_eq!(res2.rows.len(), 0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 20. Large range scans across multiple leaf pages
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_20_large_range_scans_across_multiple_leaf_pages() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("t20.treedb");
    let mut db = TreeDb::create(&db_path, 256)?;

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("k", DataType::Int64, false),
            Column::new("payload", DataType::String, false),
            Column::new("unindexed", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("big_table", schema)?;
    txn.execute_statement("CREATE INDEX idx_big ON big_table (k) INCLUDE (payload);")?;

    // Insert 600 rows to span multiple leaf pages (page size 4096 bytes, each leaf entry ~50-80 bytes)
    for i in 1..=600 {
        txn.execute_statement(&format!(
            "INSERT INTO big_table VALUES ({}, {}, 'payload_{:05}', 'unindexed_{:05}');",
            i, i, i, i
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT k, payload FROM big_table WHERE k >= 100 AND k <= 500 ORDER BY k;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 401); // 100 to 500 inclusive
    let scan = find_table_scan_metric(metrics.root_operator.as_ref().unwrap()).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 401);

    // Compare with reference scan
    for (idx, row) in res.rows.iter().enumerate() {
        let expected_k = (100 + idx) as i64;
        assert_eq!(row.values[0], Value::Int64(expected_k));
        assert_eq!(
            row.values[1],
            Value::String(format!("payload_{:05}", expected_k))
        );
    }
    Ok(())
}
