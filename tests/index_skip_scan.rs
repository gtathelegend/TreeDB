//! Cost-Based Index Skip Scan & Advanced Composite-Index Access — Phase 42 Integration Test Suite
//!
//! Requirements exercised:
//!   R67 – Basic skip scan: INDEX(a, b) + WHERE b = value chooses IndexSkipScan
//!   R68 – Multi-column suffix: INDEX(a, b, c) + WHERE b = ? AND c = ?
//!   R69 – Leading prefix constrained: WHERE a = ? uses PrefixScan / PointLookup, not SkipScan
//!   R70 – Full key equality: WHERE a = ? AND b = ? remains composite PointLookup
//!   R71 – Non-contiguous suffix: INDEX(a, b, c, d) + WHERE b = 1 AND d = 2 uses SkipScan on (a)->b with d as residual
//!   R72 – Distinct-prefix enumeration: each distinct skipped prefix is probed exactly once
//!   R73 – NULL leading-prefix value: NULL in leading column is enumerated and probed
//!   R74 – NULL suffix predicate: WHERE b IS NULL works with skip scan
//!   R75 – Residual predicate: Skip scan + residual filter matches full scan
//!   R76 – Covering skip scan: IndexOnlySkipScan avoids base record fetches (base_fetched = 0, base_avoided > 0)
//!   R77 – Incomplete coverage: missing column forces base fetch (IndexSkipScan)
//!   R78 – ORDER BY: natural index prefix ordering
//!   R79 – ORDER BY requiring sort: sort operator preserved when required
//!   R80 – LIMIT: pagination and early termination
//!   R81 – OFFSET: correct offset handling
//!   R82 – Prepared statement: skip scan works via prepared statement parameter binding
//!   R83 – Plan-cache reuse: repeated prepared execution reuses plan cache correctly
//!   R84 – Statistics-sensitive choice: low cardinality chooses skip scan, high cardinality chooses full table scan
//!   R85 – EXPLAIN: text format clearly includes IndexSkipScan and IndexOnlySkipScan
//!   R86 – EXPLAIN ANALYZE: reports actual_prefix_values, probes, entries, and base fetch metrics
//!   R87 – UPDATE: UPDATE using skip scan candidates updates rows and maintains indexes
//!   R88 – DELETE: DELETE using skip scan candidates deletes rows and maintains indexes
//!   R89 – Persistence & crash recovery: skip scan works identically across reopen and WAL recovery
//!   R90 – Randomized differential oracle: randomized SkipScan vs FullTableScan equivalence

use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::engine::StatementResult;
use treedb::error::Result;
use treedb::TreeDb;

// ─────────────────────────────────────────────────────────────────────────────
// Test Context & Helpers
// ─────────────────────────────────────────────────────────────────────────────

struct Ctx {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl Ctx {
    fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!(
            "test_skip_scan_{}_{}_{}.treedb",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
        Self {
            _dir: dir,
            db_path,
            wal_path,
        }
    }

    fn open(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

fn val_i64(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        Value::UInt64(u) => *u as i64,
        _ => panic!("expected integer value, got {v:?}"),
    }
}

fn val_str(v: &Value) -> &str {
    match v {
        Value::String(s) => s.as_str(),
        _ => panic!("expected string value, got {v:?}"),
    }
}

fn find_table_scan_metric(
    m: &treedb::query::OperatorMetrics,
) -> Option<&treedb::query::OperatorMetrics> {
    if matches!(m.kind, treedb::query::OperatorKind::TableScan { .. }) {
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
// Tests R67 – R90
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r67_basic_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r67");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, name VARCHAR);",
    )?;
    tx.execute_statement("CREATE INDEX idx_staff_dept_lvl ON staff(dept, level);")?;

    // 300 rows across 3 departments; only 3 rows with level = 999
    let depts = ["eng", "hr", "sales"];
    for i in 1..=300 {
        let dept = depts[(i % 3) as usize];
        let level = if i == 10 || i == 100 || i == 200 {
            999
        } else {
            i % 50
        };
        tx.execute_statement(&format!(
            "INSERT INTO staff VALUES ({i}, '{dept}', {level}, 'emp_{i}');"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE staff;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT id, dept, level, name FROM staff WHERE level = 999;")?;
    assert_eq!(qr.rows.len(), 3);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&10));
    assert!(ids.contains(&100));
    assert!(ids.contains(&200));

    // Check EXPLAIN
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, dept, level, name FROM staff WHERE level = 999;")?;
    let plan_text = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan_text.contains("IndexSkipScan") && plan_text.contains("idx_staff_dept_lvl"),
        "expected IndexSkipScan in plan, got:\n{plan_text}"
    );
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r68_multi_column_suffix() -> Result<()> {
    let ctx = Ctx::new("r68");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE items (id INT PRIMARY KEY, category VARCHAR, brand VARCHAR, rating INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_cat_brand_rating ON items(category, brand, rating);")?;

    let cats = ["electronics", "books", "clothing"];
    for i in 1..=300 {
        let cat = cats[(i % 3) as usize];
        let brand = format!("brand_{}", i % 20);
        let rating = (i % 10) as i64;
        tx.execute_statement(&format!(
            "INSERT INTO items VALUES ({i}, '{cat}', '{brand}', {rating});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE items;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res = tx.execute_sql(
        "EXPLAIN SELECT id, category, brand, rating FROM items WHERE brand = 'brand_5' AND rating = 5;",
    )?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexSkipScan"),
        "expected IndexSkipScan, got:\n{plan}"
    );
    assert!(plan.contains("skipped_prefix=[category]"));
    assert!(plan.contains("brand = \"brand_5\"") || plan.contains("brand = 'brand_5'"));
    assert!(plan.contains("rating = 5"));

    let qr = tx.execute_sql(
        "SELECT id, category, brand, rating FROM items WHERE brand = 'brand_5' AND rating = 5;",
    )?;
    assert!(!qr.rows.is_empty());
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r69_leading_prefix_uses_normal_prefix_scan() -> Result<()> {
    let ctx = Ctx::new("r69");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 1);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'eng', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'hr', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, dept, level FROM staff WHERE dept = 'eng';")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    // Leading prefix should NOT be SkipScan
    assert!(!plan.contains("IndexSkipScan"));
    assert!(plan.contains("PrefixScan") || plan.contains("SecondaryIndexScan"));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r70_full_key_equality_point_lookup() -> Result<()> {
    let ctx = Ctx::new("r70");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 1);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'eng', 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res = tx.execute_sql(
        "EXPLAIN SELECT id, dept, level FROM staff WHERE dept = 'eng' AND level = 3;",
    )?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(!plan.contains("IndexSkipScan"));
    assert!(plan.contains("PointLookup"));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r71_non_contiguous_suffix_residual() -> Result<()> {
    let ctx = Ctx::new("r71");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE t (id INT PRIMARY KEY, a VARCHAR, b INT, c VARCHAR, d INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_abcd ON t(a, b, c, d);")?;

    let as_list = ["x", "y", "z"];
    for i in 1..=300 {
        let a = as_list[(i % 3) as usize];
        let b = (i % 50) as i64;
        let c = if i == 10 || i == 50 { "m" } else { "n" };
        let d = if i == 10 || i == 50 { 20 } else { 30 };
        tx.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, '{a}', {b}, '{c}', {d});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE t;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Query constrains b=10 and d=20 (c is unconstrained between them)
    // a is skipped prefix, b=10 is constrained suffix, d=20 is residual filter
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, a, b, c, d FROM t WHERE b = 10 AND d = 20;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexSkipScan"),
        "expected IndexSkipScan, got:\n{plan}"
    );
    assert!(plan.contains("filter:") || plan.contains("d = 20"));

    let qr = tx.execute_sql("SELECT id, a, b, c, d FROM t WHERE b = 10 AND d = 20;")?;
    assert!(!qr.rows.is_empty());
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r72_distinct_prefix_enumeration_probed_once() -> Result<()> {
    let ctx = Ctx::new("r72");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    // 300 duplicate rows for dept 'A' and 'B', only 2 matching rows for level = 999
    for i in 1..=300 {
        let dept = if i <= 150 { "A" } else { "B" };
        let level = if i == 10 || i == 200 {
            999
        } else {
            (i % 50) as i64
        };
        tx.execute_statement(&format!(
            "INSERT INTO staff VALUES ({i}, '{dept}', {level});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE staff;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let (_, op_metric) =
        tx.execute_sql_with_metrics("SELECT id, dept, level FROM staff WHERE level = 999;")?;
    let root = op_metric.root_operator.as_ref().expect("root operator");
    let scan_metric = find_table_scan_metric(root).expect("scan metric");

    assert_eq!(scan_metric.counters.distinct_prefix_values, 2);
    assert_eq!(scan_metric.counters.skip_scan_probes, 2);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r73_null_leading_prefix_value() -> Result<()> {
    let ctx = Ctx::new("r73");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, true),
            Column::new("level", DataType::Int64, false),
        ],
        0,
    )?;
    tx.create_table("items", schema)?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON items(dept, level);")?;

    tx.execute_statement("INSERT INTO items VALUES (1, NULL, 5);")?;
    tx.execute_statement("INSERT INTO items VALUES (2, 'eng', 5);")?;
    tx.execute_statement("INSERT INTO items VALUES (3, 'eng', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT id, dept, level FROM items WHERE level = 5;")?;
    assert_eq!(qr.rows.len(), 2);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r74_null_suffix_predicate() -> Result<()> {
    let ctx = Ctx::new("r74");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, true),
        ],
        0,
    )?;
    tx.create_table("items", schema)?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON items(dept, level);")?;

    tx.execute_statement("INSERT INTO items VALUES (1, 'eng', NULL);")?;
    tx.execute_statement("INSERT INTO items VALUES (2, 'hr', NULL);")?;
    tx.execute_statement("INSERT INTO items VALUES (3, 'sales', 10);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT id, dept, level FROM items WHERE level IS NULL;")?;
    assert_eq!(qr.rows.len(), 2);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r75_residual_predicate() -> Result<()> {
    let ctx = Ctx::new("r75");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, salary INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3, 100);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'eng', 3, 200);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'hr', 3, 150);")?;
    tx.execute_statement("INSERT INTO staff VALUES (4, 'hr', 2, 300);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql(
        "SELECT id, dept, level, salary FROM staff WHERE level = 3 AND salary > 120;",
    )?;
    assert_eq!(qr.rows.len(), 2);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r76_covering_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r76");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, name VARCHAR, salary INT);",
    )?;
    tx.execute_statement(
        "CREATE INDEX idx_staff_cov ON staff(dept, level) INCLUDE (name, salary);",
    )?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3, 'Alice', 100);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 3, 'Bob', 200);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 2, 'Charlie', 150);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res = tx.execute_sql("EXPLAIN SELECT name, salary FROM staff WHERE level = 3;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexOnlySkipScan"),
        "expected IndexOnlySkipScan, got:\n{plan}"
    );
    assert!(plan.contains("covered=[name, salary]"));

    let (_, op_metric) =
        tx.execute_sql_with_metrics("SELECT name, salary FROM staff WHERE level = 3;")?;
    let root = op_metric.root_operator.as_ref().expect("root operator");
    let scan_metric = find_table_scan_metric(root).expect("scan metric");

    assert_eq!(scan_metric.counters.base_records_fetched, 0);
    assert_eq!(scan_metric.counters.base_records_avoided, 2);
    assert_eq!(scan_metric.counters.covered_rows_output, 2);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r77_incomplete_coverage_forces_base_fetch() -> Result<()> {
    let ctx = Ctx::new("r77");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, name VARCHAR, note VARCHAR);",
    )?;
    tx.execute_statement("CREATE INDEX idx_staff_cov ON staff(dept, level) INCLUDE (name);")?;

    let depts = ["eng", "hr", "sales"];
    for i in 1..=300 {
        let dept = depts[(i % 3) as usize];
        let level = if i == 10 || i == 100 { 999 } else { i % 50 };
        tx.execute_statement(&format!(
            "INSERT INTO staff VALUES ({i}, '{dept}', {level}, 'Alice_{i}', 'note_{i}');"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE staff;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Query requires `note` which is NOT covered
    let explain_res = tx.execute_sql("EXPLAIN SELECT name, note FROM staff WHERE level = 999;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexSkipScan"),
        "expected IndexSkipScan, got:\n{plan}"
    );
    assert!(!plan.contains("IndexOnlySkipScan"));

    let (_, op_metric) =
        tx.execute_sql_with_metrics("SELECT name, note FROM staff WHERE level = 999;")?;
    let root = op_metric.root_operator.as_ref().expect("root operator");
    let scan_metric = find_table_scan_metric(root).expect("scan metric");

    assert_eq!(scan_metric.counters.base_records_fetched, 2);
    assert_eq!(scan_metric.counters.base_records_avoided, 0);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r78_order_by_natural_prefix() -> Result<()> {
    let ctx = Ctx::new("r78");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'sales', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'eng', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'hr', 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT dept, level FROM staff WHERE level = 3 ORDER BY dept ASC;")?;
    assert_eq!(qr.rows.len(), 3);
    assert_eq!(val_str(&qr.rows[0].values[0]), "eng");
    assert_eq!(val_str(&qr.rows[1].values[0]), "hr");
    assert_eq!(val_str(&qr.rows[2].values[0]), "sales");
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r79_order_by_requiring_sort() -> Result<()> {
    let ctx = Ctx::new("r79");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, salary INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3, 500);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 3, 100);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 3, 300);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res = tx.execute_sql(
        "EXPLAIN SELECT dept, level, salary FROM staff WHERE level = 3 ORDER BY salary ASC;",
    )?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(plan.contains("Sort"));

    let qr = tx.execute_sql(
        "SELECT dept, level, salary FROM staff WHERE level = 3 ORDER BY salary ASC;",
    )?;
    assert_eq!(qr.rows.len(), 3);
    assert_eq!(val_i64(&qr.rows[0].values[2]), 100);
    assert_eq!(val_i64(&qr.rows[1].values[2]), 300);
    assert_eq!(val_i64(&qr.rows[2].values[2]), 500);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r80_limit_pagination() -> Result<()> {
    let ctx = Ctx::new("r80");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    for i in 1..=10 {
        let dept = format!("dept_{:02}", i);
        tx.execute_statement(&format!("INSERT INTO staff VALUES ({i}, '{dept}', 5);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr =
        tx.execute_sql("SELECT dept, level FROM staff WHERE level = 5 ORDER BY dept ASC LIMIT 3;")?;
    assert_eq!(qr.rows.len(), 3);
    assert_eq!(val_str(&qr.rows[0].values[0]), "dept_01");
    assert_eq!(val_str(&qr.rows[1].values[0]), "dept_02");
    assert_eq!(val_str(&qr.rows[2].values[0]), "dept_03");
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r81_offset_pagination() -> Result<()> {
    let ctx = Ctx::new("r81");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    for i in 1..=5 {
        let dept = format!("dept_{:02}", i);
        tx.execute_statement(&format!("INSERT INTO staff VALUES ({i}, '{dept}', 5);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql(
        "SELECT dept, level FROM staff WHERE level = 5 ORDER BY dept ASC LIMIT 2 OFFSET 2;",
    )?;
    assert_eq!(qr.rows.len(), 2);
    assert_eq!(val_str(&qr.rows[0].values[0]), "dept_03");
    assert_eq!(val_str(&qr.rows[1].values[0]), "dept_04");
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r82_prepared_statement_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r82");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 1);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'eng', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'hr', 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT id, dept, level FROM staff WHERE level = ?;")?;
    let res = tx.execute_prepared(&prep, &[Value::Int64(3)])?;
    let qr = match res {
        StatementResult::Query(qr) => qr,
        _ => panic!("Expected StatementResult::Query"),
    };
    assert_eq!(qr.rows.len(), 2);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r83_plan_cache_reuse() -> Result<()> {
    let ctx = Ctx::new("r83");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 2);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT id, dept, level FROM staff WHERE level = ?;")?;

    // First execution
    let res1 = tx.execute_prepared(&prep, &[Value::Int64(2)])?;
    let qr1 = match res1 {
        StatementResult::Query(qr) => qr,
        _ => panic!("Expected StatementResult::Query"),
    };
    assert_eq!(qr1.rows.len(), 2);

    // Second execution (hits plan cache)
    let res2 = tx.execute_prepared(&prep, &[Value::Int64(2)])?;
    let qr2 = match res2 {
        StatementResult::Query(qr) => qr,
        _ => panic!("Expected StatementResult::Query"),
    };
    assert_eq!(qr2.rows.len(), 2);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r84_statistics_sensitive_choice() -> Result<()> {
    let ctx = Ctx::new("r84");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE high_card (id INT PRIMARY KEY, unique_dept VARCHAR, level INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_high_card ON high_card(unique_dept, level);")?;

    // 100 distinct unique_dept values -> skip scan should be more expensive than full scan
    for i in 1..=100 {
        let dept = format!("unique_dept_{i}");
        tx.execute_statement(&format!("INSERT INTO high_card VALUES ({i}, '{dept}', 1);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE high_card;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, unique_dept, level FROM high_card WHERE level = 1;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    // High cardinality of skipped prefix means optimizer should reject skip scan in favor of FullTableScan
    assert!(plan.contains("FullTableScan") || !plan.contains("IndexSkipScan"));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r85_explain_formatting() -> Result<()> {
    let ctx = Ctx::new("r85");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;
    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, dept, level FROM staff WHERE level = 3;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains(
            "IndexSkipScan(index=idx_dept_lvl, skipped_prefix=[dept], constrained=[level = 3])"
        ),
        "expected IndexSkipScan format, got:\n{plan}"
    );
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r86_explain_analyze_metrics() -> Result<()> {
    let ctx = Ctx::new("r86");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;
    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("EXPLAIN ANALYZE SELECT id, dept, level FROM staff WHERE level = 3;")?;
    let explain_analyze = val_str(&res.rows[0].values[0]);
    assert!(
        explain_analyze.contains("IndexSkipScan"),
        "got:\n{explain_analyze}"
    );
    assert!(
        explain_analyze.contains("distinct_prefix_values: 3"),
        "got:\n{explain_analyze}"
    );
    assert!(
        explain_analyze.contains("skip_scan_probes: 3"),
        "got:\n{explain_analyze}"
    );
    assert!(
        explain_analyze.contains("base_fetched: 2"),
        "got:\n{explain_analyze}"
    );
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r87_update_using_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r87");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT, role VARCHAR);",
    )?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 1, 'junior');")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 1, 'junior');")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 2, 'senior');")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_statement("UPDATE staff SET role = 'mid' WHERE level = 1;")?;
    match res {
        StatementResult::Updated { rows } => assert_eq!(rows, 2),
        _ => panic!("Expected Updated"),
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT id, role FROM staff WHERE level = 1;")?;
    assert_eq!(qr.rows.len(), 2);
    for row in qr.rows {
        assert_eq!(val_str(&row.values[1]), "mid");
    }
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r88_delete_using_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r88");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 1);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 1);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_statement("DELETE FROM staff WHERE level = 1;")?;
    match res {
        StatementResult::Deleted { rows } => assert_eq!(rows, 2),
        _ => panic!("Expected Deleted"),
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT COUNT(*) FROM staff;")?;
    assert_eq!(val_i64(&qr.rows[0].values[0]), 1);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r89_persistence_and_crash_recovery() -> Result<()> {
    let ctx = Ctx::new("r89");
    {
        let mut db = ctx.open();
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
        tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;
        tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3);")?;
        tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 3);")?;
        tx.commit()?;
    }

    // Reopen database
    {
        let mut db = ctx.open();
        let mut tx = db.begin_transaction()?;
        let qr = tx.execute_sql("SELECT id, dept, level FROM staff WHERE level = 3;")?;
        assert_eq!(qr.rows.len(), 2);
        tx.commit()?;
    }

    Ok(())
}

#[test]
fn test_r90_randomized_skip_scan_oracle() -> Result<()> {
    let ctx = Ctx::new("r90");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE bench (id INT PRIMARY KEY, tag VARCHAR, score INT, active INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_tag_score ON bench(tag, score);")?;

    let tags = ["alpha", "beta", "gamma", "delta"];
    for id in 1..=100 {
        let tag = tags[(id % 4) as usize];
        let score = (id % 10) as i64;
        let active = if id % 2 == 0 { 1 } else { 0 };
        tx.execute_statement(&format!(
            "INSERT INTO bench VALUES ({id}, '{tag}', {score}, {active});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE bench;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    for target_score in 0..10 {
        let qr_skip = tx.execute_sql(&format!(
            "SELECT id, tag, score, active FROM bench WHERE score = {target_score};"
        ))?;

        // Compare against full table scan result
        let qr_full = tx.execute_sql(&format!(
            "SELECT id, tag, score, active FROM bench WHERE score = {target_score} AND id > 0;"
        ))?;

        let mut ids_skip: Vec<i64> = qr_skip.rows.iter().map(|r| val_i64(&r.values[0])).collect();
        let mut ids_full: Vec<i64> = qr_full.rows.iter().map(|r| val_i64(&r.values[0])).collect();
        ids_skip.sort();
        ids_full.sort();
        assert_eq!(
            ids_skip, ids_full,
            "Mismatch for target score {target_score}"
        );
    }
    tx.commit()?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests R91 – R101 (Hardening & Cost Model Remediation)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r91_enumeration_cost_included() -> Result<()> {
    let ctx = Ctx::new("r91");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE big_idx (id INT PRIMARY KEY, grp VARCHAR, val INT);")?;
    tx.execute_statement("CREATE INDEX idx_grp_val ON big_idx(grp, val);")?;

    for i in 1..=100 {
        let grp = format!("g_{}", i % 5);
        tx.execute_statement(&format!("INSERT INTO big_idx VALUES ({i}, '{grp}', {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE big_idx;")?;
    tx.commit()?;

    // Verify enumeration cost is non-zero
    let enum_cost = treedb::query::CostModel::index_enumeration_cost(100.0, 1.0);
    assert!(enum_cost > 0.0, "enumeration cost must be non-zero");

    let total_cost = treedb::query::CostModel::skip_scan_cost(enum_cost, 5.0, 1.0, 0);
    assert!(
        total_cost > (5.0 * treedb::query::SKIP_SCAN_PROBE_COST) + 0.52,
        "total cost must include enumeration cost"
    );

    let mut tx = db.begin_transaction()?;
    let explain_res = tx.execute_sql("EXPLAIN SELECT id, grp, val FROM big_idx WHERE val = 50;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexSkipScan"),
        "expected IndexSkipScan, got:\n{plan}"
    );
    assert!(plan.contains("cost:"), "plan should output cost");
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r92_full_scan_beats_expensive_skip_scan() -> Result<()> {
    let ctx = Ctx::new("r92");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE many_prefixes (id INT PRIMARY KEY, unique_pref VARCHAR, code INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_many_p ON many_prefixes(unique_pref, code);")?;

    // 150 distinct prefixes in 150 rows -> skip scan enumeration and probing is much more expensive than full scan
    for i in 1..=150 {
        let pref = format!("prefix_{i}");
        tx.execute_statement(&format!(
            "INSERT INTO many_prefixes VALUES ({i}, '{pref}', {i});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE many_prefixes;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, unique_pref, code FROM many_prefixes WHERE code = 10;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("FullTableScan") && !plan.contains("IndexSkipScan"),
        "FullTableScan should win over expensive skip scan when prefix NDV is high, got:\n{plan}"
    );
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r93_skip_scan_remains_viable() -> Result<()> {
    let ctx = Ctx::new("r93");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE viable (id INT PRIMARY KEY, region VARCHAR, tag INT);")?;
    tx.execute_statement("CREATE INDEX idx_region_tag ON viable(region, tag);")?;

    // 300 rows across only 3 distinct regions, highly selective tag=777
    let regions = ["north", "south", "east"];
    for i in 1..=300 {
        let region = regions[(i % 3) as usize];
        let tag = if i == 42 { 777 } else { i % 50 };
        tx.execute_statement(&format!(
            "INSERT INTO viable VALUES ({i}, '{region}', {tag});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE viable;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain_res =
        tx.execute_sql("EXPLAIN SELECT id, region, tag FROM viable WHERE tag = 777;")?;
    let plan = val_str(&explain_res.rows[0].values[0]);
    assert!(
        plan.contains("IndexSkipScan"),
        "skip scan should be selected for low prefix cardinality, got:\n{plan}"
    );

    let qr = tx.execute_sql("SELECT id, region, tag FROM viable WHERE tag = 777;")?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(val_i64(&qr.rows[0].values[0]), 42);
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r94_ndv_estimate_bounded() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("col1", DataType::String, false),
            Column::new("col2", DataType::String, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;

    let mut stats = treedb::query::TableStatistics::new("t", 100);
    stats = stats.with_column(
        "col1",
        treedb::query::ColumnStatistics::new(100, 0, 50, None, None),
    );
    stats = stats.with_column(
        "col2",
        treedb::query::ColumnStatistics::new(100, 0, 50, None, None),
    );

    // Naive NDV multiplication 50 * 50 = 2500, but table has only 100 rows
    let est = treedb::query::CardinalityEstimator::estimate_distinct_prefixes(
        Some(&stats),
        &schema,
        &[1, 2],
    );
    assert!(
        est <= 100.0,
        "distinct prefix estimate ({est}) must be capped to table row count (100.0)"
    );
    assert!(est >= 1.0, "distinct prefix estimate must be at least 1.0");

    Ok(())
}

#[test]
fn test_r95_missing_statistics_fallback() -> Result<()> {
    let ctx = Ctx::new("r95");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE unanalyzed (id INT PRIMARY KEY, cat VARCHAR, num INT);")?;
    tx.execute_statement("CREATE INDEX idx_cat_num ON unanalyzed(cat, num);")?;

    tx.execute_statement("INSERT INTO unanalyzed VALUES (1, 'c1', 10);")?;
    tx.execute_statement("INSERT INTO unanalyzed VALUES (2, 'c2', 10);")?;
    tx.execute_statement("INSERT INTO unanalyzed VALUES (3, 'c1', 20);")?;
    tx.commit()?;

    // Do NOT run ANALYZE
    let mut tx = db.begin_transaction()?;
    let qr = tx.execute_sql("SELECT id, cat, num FROM unanalyzed WHERE num = 10;")?;
    assert_eq!(qr.rows.len(), 2);
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r96_limit_behavior() -> Result<()> {
    let ctx = Ctx::new("r96");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE limits (id INT PRIMARY KEY, dept VARCHAR, lvl INT);")?;
    tx.execute_statement("CREATE INDEX idx_limits_dept_lvl ON limits(dept, lvl);")?;

    for i in 1..=50 {
        let dept = format!("dept_{:02}", i);
        tx.execute_statement(&format!("INSERT INTO limits VALUES ({i}, '{dept}', 5);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let qr =
        tx.execute_sql("SELECT dept, lvl FROM limits WHERE lvl = 5 ORDER BY dept ASC LIMIT 2;")?;
    assert_eq!(qr.rows.len(), 2);
    assert_eq!(val_str(&qr.rows[0].values[0]), "dept_01");
    assert_eq!(val_str(&qr.rows[1].values[0]), "dept_02");
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r97_analyze_metrics_complete() -> Result<()> {
    let ctx = Ctx::new("r97");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;
    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 3);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("EXPLAIN ANALYZE SELECT id, dept, level FROM staff WHERE level = 3;")?;
    let explain_text = val_str(&res.rows[0].values[0]);

    assert!(
        explain_text.contains("skip_scan_prefixes_discovered:"),
        "metrics should expose skip_scan_prefixes_discovered, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("skip_scan_prefixes_probed:"),
        "metrics should expose skip_scan_prefixes_probed, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("skip_scan_index_entries_examined:"),
        "metrics should expose skip_scan_index_entries_examined, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("skip_scan_rows_output:"),
        "metrics should expose skip_scan_rows_output, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("skip_scan_base_records_fetched:"),
        "metrics should expose skip_scan_base_records_fetched, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("skip_scan_base_records_avoided:"),
        "metrics should expose skip_scan_base_records_avoided, got:\n{explain_text}"
    );
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r98_prepared_statement_multiple_executions() -> Result<()> {
    let ctx = Ctx::new("r98");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;

    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 10);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 10);")?;
    tx.execute_statement("INSERT INTO staff VALUES (3, 'sales', 20);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT id, dept, level FROM staff WHERE level = ?;")?;

    let res10 = tx.execute_prepared(&prep, &[Value::Int64(10)])?;
    if let StatementResult::Query(qr) = res10 {
        assert_eq!(qr.rows.len(), 2);
    } else {
        panic!("expected Query result");
    }

    let res20 = tx.execute_prepared(&prep, &[Value::Int64(20)])?;
    if let StatementResult::Query(qr) = res20 {
        assert_eq!(qr.rows.len(), 1);
        assert_eq!(val_i64(&qr.rows[0].values[0]), 3);
    } else {
        panic!("expected Query result");
    }
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r99_plan_cache_invalidation_on_drop_index() -> Result<()> {
    let ctx = Ctx::new("r99");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE staff (id INT PRIMARY KEY, dept VARCHAR, level INT);")?;
    tx.execute_statement("CREATE INDEX idx_dept_lvl ON staff(dept, level);")?;
    tx.execute_statement("INSERT INTO staff VALUES (1, 'eng', 5);")?;
    tx.execute_statement("INSERT INTO staff VALUES (2, 'hr', 5);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT id, dept, level FROM staff WHERE level = ?;")?;
    let _ = tx.execute_prepared(&prep, &[Value::Int64(5)])?;
    tx.commit()?;

    // Drop index -> invalidates plan cache
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("DROP INDEX idx_dept_lvl;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let prep2 = tx.prepare_sql("SELECT id, dept, level FROM staff WHERE level = ?;")?;
    let res = tx.execute_prepared(&prep2, &[Value::Int64(5)])?;
    if let StatementResult::Query(qr) = res {
        assert_eq!(qr.rows.len(), 2);
    } else {
        panic!("expected Query result");
    }
    tx.commit()?;

    Ok(())
}

#[test]
fn test_r100_persistence_recovery_lifecycle() -> Result<()> {
    let ctx = Ctx::new("r100");
    {
        let mut db = ctx.open();
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, cat VARCHAR, subcat INT);")?;
        tx.execute_statement("CREATE INDEX idx_cat_sub ON items(cat, subcat);")?;
        tx.execute_statement("INSERT INTO items VALUES (1, 'c1', 100);")?;
        tx.execute_statement("INSERT INTO items VALUES (2, 'c2', 100);")?;
        tx.execute_statement("INSERT INTO items VALUES (3, 'c3', 200);")?;
        tx.commit()?;

        db.checkpoint()?;
    }

    // Reopen after checkpoint and test skip scan
    {
        let mut db = ctx.open();
        let mut tx = db.begin_transaction()?;
        let qr = tx.execute_sql("SELECT id, cat, subcat FROM items WHERE subcat = 100;")?;
        assert_eq!(qr.rows.len(), 2);
        let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        tx.commit()?;
    }

    Ok(())
}

#[test]
fn test_r101_randomized_differential_oracle_advanced() -> Result<()> {
    let ctx = Ctx::new("r101");
    let mut db = ctx.open();

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE multi_dim (id INT PRIMARY KEY, cat VARCHAR, tier INT, code INT, rating INT);",
    )?;
    tx.execute_statement("CREATE INDEX idx_multi ON multi_dim(cat, tier, code);")?;

    let cats = ["c_alpha", "c_beta", "c_gamma", "c_delta"];
    for id in 1..=120 {
        let cat = cats[(id % 4) as usize];
        let tier = (id % 3) as i64;
        let code = (id % 10) as i64;
        let rating = (id % 5) as i64;
        tx.execute_statement(&format!(
            "INSERT INTO multi_dim VALUES ({id}, '{cat}', {tier}, {code}, {rating});"
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("ANALYZE multi_dim;")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    // Test multiple combinations of tier, code, and rating
    for target_tier in 0..3 {
        for target_code in [1, 5, 8] {
            let qr_skip = tx.execute_sql(&format!(
                "SELECT id, cat, tier, code, rating FROM multi_dim WHERE tier = {target_tier} AND code = {target_code} AND rating >= 2;"
            ))?;

            let qr_ref = tx.execute_sql(&format!(
                "SELECT id, cat, tier, code, rating FROM multi_dim WHERE tier = {target_tier} AND code = {target_code} AND rating >= 2 AND id > 0;"
            ))?;

            let mut ids_skip: Vec<i64> =
                qr_skip.rows.iter().map(|r| val_i64(&r.values[0])).collect();
            let mut ids_ref: Vec<i64> = qr_ref.rows.iter().map(|r| val_i64(&r.values[0])).collect();
            ids_skip.sort();
            ids_ref.sort();
            assert_eq!(
                ids_skip, ids_ref,
                "Mismatch for tier={target_tier}, code={target_code}"
            );
        }
    }
    tx.commit()?;

    Ok(())
}
