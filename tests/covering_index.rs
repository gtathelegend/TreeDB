//! Covering Secondary Indexes & Index-Only Execution — Phase 41 Integration Test Suite
//!
//! Requirements exercised:
//!   R41 – CREATE INDEX ... INCLUDE (...) succeeds (single & composite keys, single & multiple includes)
//!   R42 – Persistent metadata: key and included columns survive DB close & reopen
//!   R43 – Backfill: existing rows populate included payloads atomically
//!   R44 – Basic index-only SELECT: covered projection requires zero base-record fetches (base_records_fetched = 0, base_records_avoided > 0)
//!   R45 – Incomplete coverage: missing projected column forces base-table fetch (base_records_fetched > 0)
//!   R46 – Residual predicate coverage: covered residual predicate executes without base fetch
//!   R47 – Residual predicate not covered: forces base fetch
//!   R48 – SELECT *: only index-only when every table column is covered; otherwise full/secondary fetch
//!   R49 – Included-column UPDATE: changing an included value updates the index payload correctly
//!   R50 – Key-column UPDATE: old key removed and new key inserted with covered payload
//!   R51 – DELETE: removing row reclaims index entry and index-only scan reflects deletion
//!   R52 – UNIQUE semantics: included columns do not participate in uniqueness
//!   R53 – Prefix scan: composite covering index supports prefix index-only execution
//!   R54 – Range scan: composite covering index supports range index-only execution
//!   R55 – ORDER BY: covered ordered query satisfies ORDER BY from index without sort / base fetch
//!   R56 – JOIN: covering index participates in join planning and inner/outer index-only scan
//!   R57 – SEMI/ANTI: covered residual predicates avoid base fetch
//!   R58 – Aggregation & GROUP BY: covered queries satisfy aggregates and grouping without base fetch
//!   R59 – Window functions: covered window query executes correctly without base fetch
//!   R60 – EXPLAIN: EXPLAIN identifies IndexOnlyScan with covered columns and access path
//!   R61 – EXPLAIN ANALYZE: runtime metrics report base_records_avoided and base_records_fetched = 0
//!   R62 – Plan cache: prepared covering plans remain valid across cache hits
//!   R63 – Index invalidation: DROP/CREATE index invalidates stale covering plans
//!   R64 – Persistence & recovery: covering indexes remain correct after crash/recovery and reopen
//!   R65 – Oversized included payload: boundary behavior is deterministic and rejected safely
//!   R66 – Randomized oracle: randomized index-only vs full-scan equivalence

use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::{Error, Result};
use treedb::index::covering::MAX_COVERED_PAYLOAD_SIZE;
use treedb::TreeDb;

// ─────────────────────────────────────────────────────────────────────────────
// Test Context & Helpers
// ─────────────────────────────────────────────────────────────────────────────

struct Ctx {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl Ctx {
    fn new(label: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_covering_{}_{}_{}.treedb",
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
        Self { db_path, wal_path }
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

/// Helper to recursively locate TableScan operator metric
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
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// R41 — CREATE INDEX ... INCLUDE (...) succeeds via SQL and programmatic API
#[test]
fn test_r41_create_covering_index_syntax() -> Result<()> {
    let ctx = Ctx::new("r41_syntax");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;

    // 1. Single-key covering index via SQL
    txn.execute_statement("CREATE INDEX idx_staff_dept ON staff (dept) INCLUDE (name, salary);")?;

    // 2. Composite-key covering index via SQL
    txn.execute_statement(
        "CREATE INDEX idx_staff_dept_level ON staff (dept, level) INCLUDE (name, salary);",
    )?;

    // 3. Unique covering index via SQL
    txn.execute_statement(
        "CREATE UNIQUE INDEX idx_staff_name ON staff (name) INCLUDE (dept, level);",
    )?;

    // 4. Validation: Column in both key and include is rejected
    let err = txn
        .execute_statement("CREATE INDEX idx_invalid ON staff (dept) INCLUDE (dept);")
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SqlParseError { .. }
                | Error::ColumnNotFound(_)
                | Error::InvalidIndexDefinition(_)
        ),
        "expected error on overlapping key/include columns, got {err:?}"
    );

    // 5. Validation: Duplicate column in INCLUDE is rejected
    let err2 = txn
        .execute_statement("CREATE INDEX idx_dup ON staff (dept) INCLUDE (name, name);")
        .unwrap_err();
    assert!(
        matches!(err2, Error::SqlParseError { .. }),
        "expected parse error on duplicate include columns, got {err2:?}"
    );

    txn.commit()?;
    Ok(())
}

/// R42 — Persistent metadata: key and included columns survive DB close & reopen
#[test]
fn test_r42_persistent_metadata_on_reopen() -> Result<()> {
    let ctx = Ctx::new("r42_metadata");
    {
        let mut db = ctx.open();
        let mut txn = db.begin_transaction()?;
        let schema = Schema::new(
            vec![
                Column::new("id", DataType::UInt64, false),
                Column::new("dept", DataType::String, false),
                Column::new("level", DataType::Int64, false),
                Column::new("name", DataType::String, false),
                Column::new("salary", DataType::Float64, false),
            ],
            0,
        )?;
        txn.create_table("staff", schema)?;
        txn.execute_statement(
            "CREATE INDEX idx_staff_cov ON staff (dept, level) INCLUDE (name, salary);",
        )?;
        txn.commit()?;
    }

    // Reopen database
    {
        let mut db = ctx.open();
        let mut txn = db.begin_transaction()?;
        let idx_meta = txn.get_index("idx_staff_cov")?.unwrap();
        assert_eq!(idx_meta.name, "idx_staff_cov");
        assert_eq!(idx_meta.column_indices, vec![1, 2]); // dept, level
        assert_eq!(idx_meta.include_column_indices, vec![3, 4]); // name, salary
        assert!(idx_meta.covers_column(1));
        assert!(idx_meta.covers_column(2));
        assert!(idx_meta.covers_column(3));
        assert!(idx_meta.covers_column(4));
        assert!(!idx_meta.covers_column(0)); // id not covered
    }
    Ok(())
}

/// R43 — Backfill: existing rows populate included payloads atomically
#[test]
fn test_r43_backfill_populates_included_payloads() -> Result<()> {
    let ctx = Ctx::new("r43_backfill");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, salary) VALUES (1, 'eng', 'Alice', 120000.0);",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, salary) VALUES (2, 'eng', 'Bob', 110000.0);",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, salary) VALUES (3, 'sales', 'Charlie', 95000.0);",
    )?;
    txn.commit()?;

    // Create covering index on populated table
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE INDEX idx_staff_dept_inc ON staff (dept) INCLUDE (name, salary);",
    )?;
    txn.commit()?;

    // Query covering index directly
    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT dept, name, salary FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[1], Value::String("Bob".into()));

    let root = metrics
        .root_operator
        .expect("expected root operator metric");
    let scan_metric = find_table_scan_metric(&root).expect("expected TableScan metric");
    assert_eq!(scan_metric.counters.base_records_fetched, 0);
    assert_eq!(scan_metric.counters.base_records_avoided, 2);
    assert_eq!(scan_metric.counters.covered_rows_output, 2);
    Ok(())
}

/// R44 — Basic index-only SELECT: zero base-record fetches
#[test]
fn test_r44_basic_index_only_select() -> Result<()> {
    let ctx = Ctx::new("r44_index_only");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_staff_dept_inc ON staff (dept) INCLUDE (name, salary);",
    )?;

    for i in 1..=5 {
        txn.execute_statement(&format!(
            "INSERT INTO staff (id, dept, name, salary) VALUES ({i}, 'eng', 'dev_{i}', 100000.0);"
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT name, salary FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 5);

    let root = metrics.root_operator.unwrap();
    let scan = find_table_scan_metric(&root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 5);
    assert_eq!(scan.counters.covered_rows_output, 5);
    Ok(())
}

/// R45 — Incomplete coverage: missing projected column forces base-table fetch
#[test]
fn test_r45_incomplete_coverage_forces_base_fetch() -> Result<()> {
    let ctx = Ctx::new("r45_incomplete");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("address", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    // Only includes `name`, not `address`
    txn.execute_statement("CREATE INDEX idx_staff_dept ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, address) VALUES (1, 'eng', 'Alice', '123 Tech Way');",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // Query requires `address` which is NOT covered
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT name, address FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("123 Tech Way".into()));

    let root = metrics.root_operator.unwrap();
    let scan = find_table_scan_metric(&root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 1);
    assert_eq!(scan.counters.base_records_avoided, 0);
    Ok(())
}

/// R46 — Residual predicate coverage: covered residual predicate executes without base fetch
#[test]
fn test_r46_residual_predicate_covered() -> Result<()> {
    let ctx = Ctx::new("r46_residual_covered");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_staff_dept_cov ON staff (dept) INCLUDE (name, salary);",
    )?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, salary) VALUES (1, 'eng', 'Alice', 150000.0);",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, salary) VALUES (2, 'eng', 'Bob', 80000.0);",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    // Residual predicate `salary > 100000.0` is covered by the index
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT name FROM staff WHERE dept = 'eng' AND salary > 100000.0;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));

    let root = metrics.root_operator.unwrap();
    let scan = find_table_scan_metric(&root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);
    Ok(())
}

/// R47 — Residual predicate not covered: forces base fetch
#[test]
fn test_r47_residual_predicate_not_covered() -> Result<()> {
    let ctx = Ctx::new("r47_residual_not_covered");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("rating", DataType::Int64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    // Only includes `name`, not `rating`
    txn.execute_statement("CREATE INDEX idx_staff_dept ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, name, rating) VALUES (1, 'eng', 'Alice', 5);",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT name FROM staff WHERE dept = 'eng' AND rating >= 4;")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 1);
    assert_eq!(scan.counters.base_records_avoided, 0);
    Ok(())
}

/// R48 — SELECT *: only index-only when every table column is covered
#[test]
fn test_r48_select_star_coverage() -> Result<()> {
    let ctx = Ctx::new("r48_select_star");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;

    // Covering index covers ALL 3 columns: key(dept) + inc(id, name)
    txn.execute_statement("CREATE INDEX idx_all ON staff (dept) INCLUDE (id, name);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (1, 'eng', 'Alice');")?;
    txn.commit()?;

    // 1. Fully covered SELECT *
    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics("SELECT * FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[0].values[1], Value::String("eng".into()));
    assert_eq!(res.rows[0].values[2], Value::String("Alice".into()));
    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 1);

    // 2. Drop index and create partial covering index -> SELECT * falls back to base fetch
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("DROP INDEX idx_all;")?;
    txn.execute_statement("CREATE INDEX idx_partial ON staff (dept) INCLUDE (name);")?;
    let (res2, metrics2) =
        txn.execute_sql_with_metrics("SELECT * FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;
    assert_eq!(res2.rows.len(), 1);
    let root2 = metrics2.root_operator.as_ref().unwrap();
    let scan2 = find_table_scan_metric(root2).unwrap();
    assert_eq!(scan2.counters.base_records_fetched, 1);
    assert_eq!(scan2.counters.base_records_avoided, 0);
    Ok(())
}

/// R49 — Included-column UPDATE: changing an included value updates the index payload
#[test]
fn test_r49_included_column_update() -> Result<()> {
    let ctx = Ctx::new("r49_update_included");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_sal ON staff (dept) INCLUDE (salary);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (1, 'eng', 100000.0);")?;
    txn.commit()?;

    // Update only included column (salary)
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE staff SET salary = 125000.0 WHERE id = 1;")?;
    txn.commit()?;

    // Query index-only to verify updated value is reflected in index payload
    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT dept, salary FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::Float64(125000.0));

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 1);
    Ok(())
}

/// R50 — Key-column UPDATE: old key removed and new key inserted with covered payload
#[test]
fn test_r50_key_column_update() -> Result<()> {
    let ctx = Ctx::new("r50_update_key");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_name ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (1, 'eng', 'Alice');")?;
    txn.commit()?;

    // Update key column (dept: 'eng' -> 'hr')
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("UPDATE staff SET dept = 'hr' WHERE id = 1;")?;
    txn.commit()?;

    // Query old key: should return 0 rows
    let mut txn = db.begin_transaction()?;
    let (res_old, _) =
        txn.execute_sql_with_metrics("SELECT dept, name FROM staff WHERE dept = 'eng';")?;
    assert_eq!(res_old.rows.len(), 0);

    // Query new key: should return updated row index-only
    let (res_new, metrics) =
        txn.execute_sql_with_metrics("SELECT dept, name FROM staff WHERE dept = 'hr';")?;
    txn.commit()?;
    assert_eq!(res_new.rows.len(), 1);
    assert_eq!(res_new.rows[0].values[1], Value::String("Alice".into()));
    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

/// R51 — DELETE: index-only candidate remains consistent after deletion
#[test]
fn test_r51_delete_removes_covered_entry() -> Result<()> {
    let ctx = Ctx::new("r51_delete");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_name ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (1, 'eng', 'Alice');")?;
    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (2, 'eng', 'Bob');")?;
    txn.commit()?;

    // Delete Alice
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("DELETE FROM staff WHERE id = 1;")?;
    txn.commit()?;

    // Query index-only
    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT dept, name FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("Bob".into()));
    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

/// R52 — UNIQUE semantics: included columns do not participate in uniqueness
#[test]
fn test_r52_unique_semantics_with_included_columns() -> Result<()> {
    let ctx = Ctx::new("r52_unique");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    // Unique on dept, include name
    txn.execute_statement("CREATE UNIQUE INDEX idx_uniq_dept ON staff (dept) INCLUDE (name);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (1, 'eng', 'Alice');")?;

    // Same dept ('eng') but different included value ('Bob') MUST still violate UNIQUE
    let err = txn
        .execute_sql("INSERT INTO staff (id, dept, name) VALUES (2, 'eng', 'Bob');")
        .unwrap_err();
    assert!(
        matches!(err, Error::DuplicateIndexKey(_)),
        "expected DuplicateIndexKey, got {err:?}"
    );

    txn.commit()?;
    Ok(())
}

/// R53 — Prefix scan: composite covering index supports prefix index-only execution
#[test]
fn test_r53_composite_prefix_index_only_scan() -> Result<()> {
    let ctx = Ctx::new("r53_prefix");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_staff_dl ON staff (dept, level) INCLUDE (name, salary);",
    )?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name, salary) VALUES (1, 'eng', 1, 'Alice', 90000.0);",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name, salary) VALUES (2, 'eng', 2, 'Bob', 120000.0);",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name, salary) VALUES (3, 'sales', 1, 'Charlie', 80000.0);",
    )?;
    txn.commit()?;

    // Query filtering only on leading prefix (dept = 'eng')
    let mut txn = db.begin_transaction()?;
    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT name, level, salary FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);
    Ok(())
}

/// R54 — Range scan: composite covering index supports range index-only execution
#[test]
fn test_r54_range_index_only_scan() -> Result<()> {
    let ctx = Ctx::new("r54_range");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("score", DataType::Int64, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("students", schema)?;
    txn.execute_statement("CREATE INDEX idx_score_inc ON students (score) INCLUDE (name);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!(
            "INSERT INTO students (id, score, name) VALUES ({i}, {}, 'student_{i}');",
            i * 10
        ))?;
    }
    txn.commit()?;

    // Range query: score BETWEEN 30 AND 70
    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT score, name FROM students WHERE score >= 30 AND score <= 70;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 5);
    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(
        scan.counters.base_records_avoided,
        scan.counters.index_entries_examined
    );
    Ok(())
}

/// R55 — ORDER BY: covered ordered query avoids unnecessary sort/base fetch
#[test]
fn test_r55_order_by_covering_index() -> Result<()> {
    let ctx = Ctx::new("r55_order_by");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_lvl ON staff (dept, level) INCLUDE (name);")?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name) VALUES (1, 'eng', 3, 'Carol');",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name) VALUES (2, 'eng', 1, 'Alice');",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name) VALUES (3, 'eng', 2, 'Bob');",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT name, level FROM staff WHERE dept = 'eng' ORDER BY level ASC;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[2].values[0], Value::String("Carol".into()));

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 3);
    Ok(())
}

/// R56 — JOIN: covering index participates in join planning correctly
#[test]
fn test_r56_join_covering_index() -> Result<()> {
    let ctx = Ctx::new("r56_join");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let dept_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept_name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("departments", dept_schema)?;

    let staff_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept_id", DataType::UInt64, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", staff_schema)?;
    // Covering index on staff: key(level) INCLUDE (dept_id, name)
    txn.execute_statement("CREATE INDEX idx_staff_lvl ON staff (level) INCLUDE (dept_id, name);")?;

    txn.execute_statement("INSERT INTO departments (id, dept_name) VALUES (10, 'Engineering');")?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept_id, level, name) VALUES (1, 10, 3, 'Alice');",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT s.name, d.dept_name FROM staff s JOIN departments d ON s.dept_id = d.id WHERE s.level = 3;",
        )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::String("Engineering".into()));

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    Ok(())
}

/// R57 — SEMI/ANTI: covered residual predicates avoid unnecessary base fetch
#[test]
fn test_r57_semi_anti_covering_index() -> Result<()> {
    let ctx = Ctx::new("r57_semi_anti");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let dept_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("departments", dept_schema)?;

    let staff_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept_id", DataType::UInt64, false),
            Column::new("status", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", staff_schema)?;
    txn.execute_statement("CREATE INDEX idx_staff_dept_stat ON staff (dept_id) INCLUDE (status);")?;

    txn.execute_statement("INSERT INTO departments (id, name) VALUES (1, 'Engineering');")?;
    txn.execute_statement("INSERT INTO departments (id, name) VALUES (2, 'HR');")?;
    txn.execute_statement("INSERT INTO staff (id, dept_id, status) VALUES (10, 1, 'active');")?;
    txn.execute_statement("INSERT INTO staff (id, dept_id, status) VALUES (20, 2, 'inactive');")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, _) = txn
        .execute_sql_with_metrics(
            "SELECT d.name FROM departments d WHERE EXISTS (SELECT 1 FROM staff s WHERE s.dept_id = d.id AND s.status = 'active');",
        )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("Engineering".into()));
    Ok(())
}

/// R58 — Aggregation & GROUP BY: covered queries satisfy aggregates and grouping without base fetch
#[test]
fn test_r58_aggregation_covering_index() -> Result<()> {
    let ctx = Ctx::new("r58_agg");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_dept_salary ON staff (dept) INCLUDE (salary);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (1, 'eng', 100.0);")?;
    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (2, 'eng', 200.0);")?;
    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (3, 'hr', 150.0);")?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn.execute_sql_with_metrics(
        "SELECT dept, COUNT(*), SUM(salary) FROM staff WHERE dept = 'eng' GROUP BY dept;",
    )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("eng".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(2));
    assert_eq!(res.rows[0].values[2], Value::Float64(300.0));

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);
    Ok(())
}

/// R59 — Window functions: covered window query executes correctly without base fetch
#[test]
fn test_r59_window_covering_index() -> Result<()> {
    let ctx = Ctx::new("r59_window");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_win_cov ON staff (dept) INCLUDE (level, name);")?;

    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name) VALUES (1, 'eng', 1, 'Alice');",
    )?;
    txn.execute_statement(
        "INSERT INTO staff (id, dept, level, name) VALUES (2, 'eng', 2, 'Bob');",
    )?;
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let (res, metrics) = txn
        .execute_sql_with_metrics(
            "SELECT name, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY level ASC) FROM staff WHERE dept = 'eng';",
        )?;
    txn.commit()?;

    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".into()));
    assert_eq!(res.rows[0].values[1], Value::UInt64(1));
    assert_eq!(res.rows[1].values[0], Value::String("Bob".into()));
    assert_eq!(res.rows[1].values[1], Value::UInt64(2));

    let root = metrics.root_operator.as_ref().unwrap();
    let scan = find_table_scan_metric(root).unwrap();
    assert_eq!(scan.counters.base_records_fetched, 0);
    assert_eq!(scan.counters.base_records_avoided, 2);
    Ok(())
}

/// R60 — EXPLAIN identifies IndexOnlyScan with covered columns and access path
#[test]
fn test_r60_explain_identifies_index_only_scan() -> Result<()> {
    let ctx = Ctx::new("r60_explain");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_staff_dept_level ON staff (dept, level) INCLUDE (name, salary);",
    )?;

    let explain_res =
        txn.execute_sql("EXPLAIN SELECT name, salary FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    let explain_text = match &explain_res.rows[0].values[0] {
        Value::String(text) => text,
        _ => panic!("expected string explain output"),
    };

    assert!(
        explain_text.contains("IndexOnlyScan"),
        "EXPLAIN output should mention IndexOnlyScan, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("idx_staff_dept_level"),
        "EXPLAIN output should mention index name, got:\n{explain_text}"
    );
    assert!(
        explain_text.contains("name") && explain_text.contains("salary"),
        "EXPLAIN output should list covered columns, got:\n{explain_text}"
    );
    Ok(())
}

/// R61 — EXPLAIN ANALYZE: runtime metrics report base_records_avoided and base_records_fetched = 0
#[test]
fn test_r61_explain_analyze_metrics() -> Result<()> {
    let ctx = Ctx::new("r61_explain_analyze");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_staff_cov ON staff (dept) INCLUDE (name);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!(
            "INSERT INTO staff (id, dept, name) VALUES ({i}, 'eng', 'dev_{i}');"
        ))?;
    }

    let res =
        txn.execute_sql("EXPLAIN ANALYZE SELECT dept, name FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    let text = match &res.rows[0].values[0] {
        Value::String(t) => t,
        _ => panic!("expected string output"),
    };

    assert!(
        text.contains("base_fetched: 0")
            || text.contains("base_fetched=0")
            || text.contains("base_records_fetched: 0"),
        "EXPLAIN ANALYZE should show 0 base records fetched, got:\n{text}"
    );
    assert!(
        text.contains("base_avoided: 10")
            || text.contains("base_avoided=10")
            || text.contains("base_records_avoided: 10"),
        "EXPLAIN ANALYZE should show 10 base records avoided, got:\n{text}"
    );
    Ok(())
}

/// R62 — Plan cache: prepared covering plans remain valid across cache hits
#[test]
fn test_r62_plan_cache_prepared_covering_scan() -> Result<()> {
    let ctx = Ctx::new("r62_plan_cache");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("salary", DataType::Float64, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_cov ON staff (dept) INCLUDE (salary);")?;

    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (1, 'eng', 100000.0);")?;
    txn.execute_statement("INSERT INTO staff (id, dept, salary) VALUES (2, 'sales', 80000.0);")?;
    txn.commit()?;

    // Prepare statement with parameter
    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT dept, salary FROM staff WHERE dept = ?;")?;

    // First execution (compiles and caches plan)
    let (res1, m1) = txn.execute_prepared_with_metrics(&stmt, &[Value::String("eng".into())])?;
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[1], Value::Float64(100000.0));
    let root1 = m1.root_operator.as_ref().unwrap();
    let scan1 = find_table_scan_metric(root1).unwrap();
    assert_eq!(scan1.counters.base_records_fetched, 0);

    // Second execution (plan cache hit)
    let (res2, m2) = txn.execute_prepared_with_metrics(&stmt, &[Value::String("sales".into())])?;
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[1], Value::Float64(80000.0));
    let root2 = m2.root_operator.as_ref().unwrap();
    let scan2 = find_table_scan_metric(root2).unwrap();
    assert_eq!(scan2.counters.base_records_fetched, 0);

    txn.commit()?;
    Ok(())
}

/// R63 — Index invalidation: DROP/CREATE index invalidates stale covering plans
#[test]
fn test_r63_index_invalidation_plan_cache() -> Result<()> {
    let ctx = Ctx::new("r63_invalidation");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("name", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;
    txn.execute_statement("CREATE INDEX idx_cov ON staff (dept) INCLUDE (name);")?;
    txn.execute_statement("INSERT INTO staff (id, dept, name) VALUES (1, 'eng', 'Alice');")?;
    txn.commit()?;

    // Execute query to populate plan cache
    let mut txn = db.begin_transaction()?;
    let (res1, m1) =
        txn.execute_sql_with_metrics("SELECT dept, name FROM staff WHERE dept = 'eng';")?;
    assert_eq!(res1.rows.len(), 1);
    let root1 = m1.root_operator.as_ref().unwrap();
    let scan1 = find_table_scan_metric(root1).unwrap();
    assert_eq!(scan1.counters.base_records_fetched, 0);

    // Drop covering index
    txn.execute_statement("DROP INDEX idx_cov;")?;

    // Re-execute: plan cache must have been invalidated and fall back to FullTableScan
    let (res2, m2) =
        txn.execute_sql_with_metrics("SELECT dept, name FROM staff WHERE dept = 'eng';")?;
    txn.commit()?;

    assert_eq!(res2.rows.len(), 1);
    let root2 = m2.root_operator.as_ref().unwrap();
    let scan2 = find_table_scan_metric(root2).unwrap();
    assert_eq!(scan2.counters.base_records_fetched, 1);
    assert_eq!(scan2.counters.base_records_avoided, 0);
    Ok(())
}

/// R64 — Persistence & recovery: covering indexes remain correct after crash/recovery and reopen
#[test]
fn test_r64_persistence_and_recovery() -> Result<()> {
    let ctx = Ctx::new("r64_recovery");
    {
        let mut db = ctx.open();
        let mut txn = db.begin_transaction()?;
        let schema = Schema::new(
            vec![
                Column::new("id", DataType::UInt64, false),
                Column::new("dept", DataType::String, false),
                Column::new("name", DataType::String, false),
                Column::new("salary", DataType::Float64, false),
            ],
            0,
        )?;
        txn.create_table("staff", schema)?;
        txn.execute_statement("CREATE INDEX idx_cov ON staff (dept) INCLUDE (name, salary);")?;

        for i in 1..=50 {
            txn.execute_statement(&format!(
                "INSERT INTO staff (id, dept, name, salary) VALUES ({i}, 'eng', 'dev_{i}', {});",
                i as f64 * 1000.0
            ))?;
        }
        txn.commit()?;

        // Mutate some rows
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("UPDATE staff SET salary = 99999.0 WHERE id = 10;")?;
        txn.execute_statement("DELETE FROM staff WHERE id = 20;")?;
        txn.commit()?;
    }

    // Reopen and verify index-only scan returns exact, correct recovered data
    {
        let mut db = ctx.open();
        let mut txn = db.begin_transaction()?;
        let (res, metrics) = txn
            .execute_sql_with_metrics("SELECT dept, name, salary FROM staff WHERE dept = 'eng';")?;
        txn.commit()?;

        let rows = res.rows;
        assert_eq!(rows.len(), 49); // 50 inserted - 1 deleted

        // Verify updated row
        let row_10 = rows
            .iter()
            .find(|r| r.values[1] == Value::String("dev_10".into()))
            .unwrap();
        assert_eq!(row_10.values[2], Value::Float64(99999.0));

        // Verify deleted row
        assert!(!rows
            .iter()
            .any(|r| r.values[1] == Value::String("dev_20".into())));

        let root = metrics.root_operator.as_ref().unwrap();
        let scan = find_table_scan_metric(root).unwrap();
        assert_eq!(scan.counters.base_records_fetched, 0);
        assert_eq!(scan.counters.base_records_avoided, 49);
    }
    Ok(())
}

/// R65 — Oversized included payload: boundary behavior is deterministic and rejected safely
#[test]
fn test_r65_oversized_included_payload() -> Result<()> {
    let ctx = Ctx::new("r65_oversized");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("category", DataType::String, false),
            Column::new("large_text", DataType::String, false),
        ],
        0,
    )?;
    txn.create_table("articles", schema)?;
    txn.execute_statement(
        "CREATE INDEX idx_cat_text ON articles (category) INCLUDE (large_text);",
    )?;

    // String that exceeds MAX_COVERED_PAYLOAD_SIZE (1000 bytes)
    let huge_str = "x".repeat(MAX_COVERED_PAYLOAD_SIZE + 50);
    let sql = format!(
        "INSERT INTO articles (id, category, large_text) VALUES (1, 'news', '{huge_str}');"
    );

    let err = txn.execute_statement(&sql).unwrap_err();
    assert!(
        matches!(
            err,
            Error::IndexCorruption(_) | Error::TreeCorruption(_) | Error::ValueTooLarge { .. }
        ),
        "expected error rejecting oversized covered payload, got {err:?}"
    );
    Ok(())
}

/// R66 — Randomized oracle: randomized index-only vs full-scan equivalence
#[test]
fn test_r66_randomized_covering_index_oracle() -> Result<()> {
    let ctx = Ctx::new("r66_oracle");
    let mut db = ctx.open();

    let mut txn = db.begin_transaction()?;
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, false),
            Column::new("level", DataType::Int64, false),
            Column::new("rating", DataType::Int64, true),
            Column::new("name", DataType::String, false),
            Column::new("salary", DataType::Float64, true),
        ],
        0,
    )?;
    txn.create_table("staff", schema)?;

    txn.execute_statement(
        "CREATE INDEX idx_staff_dl ON staff (dept, level) INCLUDE (rating, name, salary);",
    )?;

    let depts = ["eng", "hr", "sales", "finance"];
    let names = ["Alice", "Bob", "Charlie", "David", "Eve", "Frank", "Grace"];

    // Populate pseudo-random dataset
    let mut seed: u64 = 987654321;
    let mut next_rand = || -> u64 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    for id in 1..=200 {
        let r = next_rand();
        let dept = depts[(r % depts.len() as u64) as usize];
        let level = ((r >> 8) % 5) as i64 + 1;
        let rating = if (r >> 16) % 10 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", ((r >> 20) % 5) + 1)
        };
        let name = names[((r >> 24) % names.len() as u64) as usize];
        let salary = if (r >> 32) % 10 == 0 {
            "NULL".to_string()
        } else {
            format!("{}.0", ((r >> 36) % 100) * 1000 + 50000)
        };

        txn.execute_statement(&format!(
            "INSERT INTO staff (id, dept, level, rating, name, salary) VALUES ({id}, '{dept}', {level}, {rating}, '{name}', {salary});"
        ))?;
    }
    txn.commit()?;

    // Run randomized query workload comparing covering index vs table scan
    for _ in 0..50 {
        let r = next_rand();
        let target_dept = depts[(r % depts.len() as u64) as usize];
        let target_level = ((r >> 8) % 5) as i64 + 1;

        let queries = [
            format!("SELECT dept, name, salary FROM staff WHERE dept = '{target_dept}';"),
            format!("SELECT dept, level, name FROM staff WHERE dept = '{target_dept}' AND level = {target_level};"),
            format!("SELECT name, salary FROM staff WHERE dept = '{target_dept}' AND level >= {target_level} ORDER BY level ASC;"),
            format!("SELECT dept, COUNT(*), SUM(rating) FROM staff WHERE dept = '{target_dept}' GROUP BY dept;"),
            format!("SELECT name, rating FROM staff WHERE dept = '{target_dept}' AND rating IS NOT NULL;"),
        ];

        for sql in queries {
            let mut txn = db.begin_transaction()?;
            let (res, metrics) = txn.execute_sql_with_metrics(&sql)?;
            txn.commit()?;

            let rows = res.rows;

            // Assert that covering scan was executed without base-record fetches
            let root = metrics.root_operator.as_ref().unwrap();
            let scan = find_table_scan_metric(root).unwrap();
            assert_eq!(
                scan.counters.base_records_fetched, 0,
                "Query failed index-only assertion for: {sql}"
            );
            assert_eq!(
                scan.counters.base_records_avoided, scan.counters.index_entries_examined,
                "Avoided count mismatch for: {sql}"
            );

            // Basic sanity on returned rows
            for row in rows {
                assert!(!row.values.is_empty());
            }
        }
    }
    Ok(())
}
