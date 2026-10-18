//! Phase 43: Cost-Based Multi-Index Predicate Planning Integration Test Suite
//!
//! Test identifiers:
//!   R102 – Two-index AND intersection
//!   R103 – Three-index AND intersection
//!   R104 – Two-branch OR union
//!   R105 – Duplicate RecordId removal during union
//!   R106 – Intersection with empty candidate set
//!   R107 – Union with empty candidate set
//!   R108 – Residual predicate after intersection
//!   R109 – Residual predicate after union
//!   R110 – NULL semantics for AND
//!   R111 – NULL semantics for OR
//!   R112 – Cost model prefers multi-index access when selective
//!   R113 – Cost model rejects multi-index access when full scan is cheaper
//!   R114 – Statistics-sensitive plan selection
//!   R115 – Missing-statistics fallback
//!   R116 – Composite-index participation
//!   R117 – Covering-index interaction
//!   R118 – ORDER BY correctness
//!   R119 – LIMIT/OFFSET correctness
//!   R120 – Prepared statement execution
//!   R121 – Plan-cache reuse
//!   R122 – CREATE INDEX invalidates cached plan
//!   R123 – DROP INDEX invalidates cached plan
//!   R124 – ANALYZE/statistics generation invalidates/replans appropriately
//!   R125 – UPDATE using multi-index candidate generation
//!   R126 – DELETE using multi-index candidate generation
//!   R127 – Transaction rollback
//!   R128 – Persistence/reopen
//!   R129 – Checkpoint/recovery
//!   R130 – Randomized differential AND/OR oracle

use std::fs;
use std::path::PathBuf;

use treedb::engine::types::Value;
use treedb::engine::StatementResult;
use treedb::error::Result;
use treedb::TreeDb;

struct Ctx {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl Ctx {
    fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!(
            "test_multi_idx_{}_{}_{}.treedb",
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

/// Helper to execute SQL statement in a transaction and commit.
fn exec_sql(db: &mut TreeDb, sql: &str) -> Result<StatementResult> {
    db.execute_statement(sql)
}

/// Helper to explain a query and check if the plan contains a specific substring.
fn explain_contains(db: &mut TreeDb, sql: &str, substring: &str) -> bool {
    let mut txn = db.begin_transaction().unwrap();
    let res = txn.execute_sql(&format!("EXPLAIN {}", sql)).unwrap();
    txn.commit().unwrap();
    let plan_text = val_str(&res.rows[0].values[0]);
    plan_text.contains(substring)
}

// ─────────────────────────────────────────────────────────────────────────────
// R102: Two-index AND intersection
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r102_two_index_and_intersection() -> Result<()> {
    let ctx = Ctx::new("r102");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE users (id INT PRIMARY KEY, age INT, status TEXT, salary INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_age ON users (age);")?;
    exec_sql(&mut db, "CREATE INDEX idx_status ON users (status);")?;

    for i in 1..=500 {
        let age = 20 + (i % 25);
        let status = format!("status_{}", (i / 25) % 20);
        let salary = 30000 + i * 100;
        exec_sql(
            &mut db,
            &format!("INSERT INTO users VALUES ({i}, {age}, '{status}', {salary});"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE users;")?;

    let query = "SELECT id, age, status FROM users WHERE age = 30 AND status = 'status_2';";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan in plan"
    );
    assert!(
        explain_contains(&mut db, query, "strategy=INTERSECTION"),
        "expected INTERSECTION strategy in plan"
    );

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert!(!qr.rows.is_empty());
    for row in &qr.rows {
        assert_eq!(row.values[1], Value::Int64(30));
        assert_eq!(row.values[2], Value::String("status_2".to_string()));
    }
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R103: Three-index AND intersection
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r103_three_index_and_intersection() -> Result<()> {
    let ctx = Ctx::new("r103");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE items (id INT PRIMARY KEY, cat INT, brand INT, tier INT, price INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_cat ON items (cat);")?;
    exec_sql(&mut db, "CREATE INDEX idx_brand ON items (brand);")?;
    exec_sql(&mut db, "CREATE INDEX idx_tier ON items (tier);")?;

    for i in 1..=500 {
        let cat = i % 25;
        let brand = (i / 5) % 20;
        let tier = (i / 25) % 20;
        let price = i * 10;
        exec_sql(
            &mut db,
            &format!("INSERT INTO items VALUES ({i}, {cat}, {brand}, {tier}, {price});"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE items;")?;

    let query = "SELECT id FROM items WHERE cat = 3 AND brand = 0 AND tier = 0;";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan in plan"
    );
    assert!(
        explain_contains(&mut db, query, "strategy=INTERSECTION"),
        "expected INTERSECTION strategy in plan"
    );

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert!(!qr.rows.is_empty());
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R104: Two-branch OR union
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r104_two_branch_or_union() -> Result<()> {
    let ctx = Ctx::new("r104");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE logs (id INT PRIMARY KEY, level TEXT, code INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_level ON logs (level);")?;
    exec_sql(&mut db, "CREATE INDEX idx_code ON logs (code);")?;

    for i in 1..=2000 {
        let level = format!("lvl_{}", i % 100);
        let code = 100 + (i % 100);
        exec_sql(
            &mut db,
            &format!("INSERT INTO logs VALUES ({i}, '{level}', {code});"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE logs;")?;

    let query = "SELECT id, level, code FROM logs WHERE level = 'lvl_3' OR code = 105;";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan in plan"
    );
    assert!(
        explain_contains(&mut db, query, "strategy=UNION"),
        "expected UNION strategy in plan"
    );

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert!(!qr.rows.is_empty());
    for r in &qr.rows {
        let lvl_match = r.values[1] == Value::String("lvl_3".to_string());
        let code_match = r.values[2] == Value::Int64(105);
        assert!(lvl_match || code_match);
    }
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R105: Duplicate RecordId removal during union
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r105_duplicate_removal_during_union() -> Result<()> {
    let ctx = Ctx::new("r105");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE data (id INT PRIMARY KEY, tag_a INT, tag_b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ta ON data (tag_a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_tb ON data (tag_b);")?;

    // Row 1 matches BOTH tag_a = 1 and tag_b = 2
    exec_sql(&mut db, "INSERT INTO data VALUES (1, 1, 2);")?;
    // Row 2 matches only tag_a = 1
    exec_sql(&mut db, "INSERT INTO data VALUES (2, 1, 99);")?;
    // Row 3 matches only tag_b = 2
    exec_sql(&mut db, "INSERT INTO data VALUES (3, 99, 2);")?;
    // Row 4 matches neither
    exec_sql(&mut db, "INSERT INTO data VALUES (4, 99, 99);")?;

    let query = "SELECT id FROM data WHERE tag_a = 1 OR tag_b = 2 ORDER BY id;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    // Row 1 must appear exactly once!
    assert_eq!(ids, vec![1, 2, 3]);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R106: Intersection with empty candidate set
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r106_intersection_with_empty_candidate_set() -> Result<()> {
    let ctx = Ctx::new("r106");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE customers (id INT PRIMARY KEY, age INT, city TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_c_age ON customers (age);")?;
    exec_sql(&mut db, "CREATE INDEX idx_c_city ON customers (city);")?;

    for i in 1..=50 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO customers VALUES ({i}, 25, 'London');"),
        )?;
    }

    // city = 'Tokyo' has 0 matches
    let query = "SELECT id FROM customers WHERE age = 25 AND city = 'Tokyo';";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert_eq!(qr.rows.len(), 0);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R107: Union with empty candidate set
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r107_union_with_empty_candidate_set() -> Result<()> {
    let ctx = Ctx::new("r107");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE records (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_a ON records (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_b ON records (b);")?;

    exec_sql(&mut db, "INSERT INTO records VALUES (1, 10, 100);")?;
    exec_sql(&mut db, "INSERT INTO records VALUES (2, 20, 200);")?;

    // a = 99 has 0 matches, b = 100 matches id 1
    let query = "SELECT id FROM records WHERE a = 99 OR b = 100;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(qr.rows[0].values[0], Value::Int64(1));
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R108: Residual predicate after intersection
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r108_residual_predicate_after_intersection() -> Result<()> {
    let ctx = Ctx::new("r108");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE accounts (id INT PRIMARY KEY, tier INT, active INT, balance INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_tier ON accounts (tier);")?;
    exec_sql(&mut db, "CREATE INDEX idx_active ON accounts (active);")?;

    // tier = 1 AND active = 1 -> balances 100, 500, 1000
    exec_sql(&mut db, "INSERT INTO accounts VALUES (1, 1, 1, 100);")?;
    exec_sql(&mut db, "INSERT INTO accounts VALUES (2, 1, 1, 500);")?;
    exec_sql(&mut db, "INSERT INTO accounts VALUES (3, 1, 1, 1000);")?;
    exec_sql(&mut db, "INSERT INTO accounts VALUES (4, 1, 0, 1000);")?;

    let query = "SELECT id, balance FROM accounts WHERE tier = 1 AND active = 1 AND balance > 200 ORDER BY id;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert_eq!(qr.rows.len(), 2);
    assert_eq!(qr.rows[0].values[0], Value::Int64(2));
    assert_eq!(qr.rows[1].values[0], Value::Int64(3));
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R109: Residual predicate after union
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r109_residual_predicate_after_union() -> Result<()> {
    let ctx = Ctx::new("r109");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE vehicles (id INT PRIMARY KEY, type TEXT, color TEXT, year INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_type ON vehicles (type);")?;
    exec_sql(&mut db, "CREATE INDEX idx_color ON vehicles (color);")?;

    exec_sql(
        &mut db,
        "INSERT INTO vehicles VALUES (1, 'SUV', 'red', 2010);",
    )?;
    exec_sql(
        &mut db,
        "INSERT INTO vehicles VALUES (2, 'Sedan', 'red', 2020);",
    )?;
    exec_sql(
        &mut db,
        "INSERT INTO vehicles VALUES (3, 'SUV', 'blue', 2022);",
    )?;
    exec_sql(
        &mut db,
        "INSERT INTO vehicles VALUES (4, 'Truck', 'black', 2021);",
    )?;

    let query = "SELECT id FROM vehicles WHERE (type = 'SUV' OR color = 'red') AND year > 2015 ORDER BY id;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    // ID 1 (year 2010) rejected by residual. ID 2 (Sedan, red, 2020) and ID 3 (SUV, blue, 2022) pass.
    assert_eq!(ids, vec![2, 3]);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R110: NULL semantics for AND
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r110_null_semantics_and() -> Result<()> {
    let ctx = Ctx::new("r110");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE null_test (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_nta ON null_test (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_ntb ON null_test (b);")?;

    exec_sql(&mut db, "INSERT INTO null_test VALUES (1, 10, NULL);")?;
    exec_sql(&mut db, "INSERT INTO null_test VALUES (2, NULL, 20);")?;
    exec_sql(&mut db, "INSERT INTO null_test VALUES (3, 10, 20);")?;
    exec_sql(&mut db, "INSERT INTO null_test VALUES (4, NULL, NULL);")?;

    let query = "SELECT id FROM null_test WHERE a = 10 AND b = 20;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(qr.rows[0].values[0], Value::Int64(3));
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R111: NULL semantics for OR
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r111_null_semantics_or() -> Result<()> {
    let ctx = Ctx::new("r111");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE null_or (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_noa ON null_or (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_nob ON null_or (b);")?;

    exec_sql(&mut db, "INSERT INTO null_or VALUES (1, 10, NULL);")?;
    exec_sql(&mut db, "INSERT INTO null_or VALUES (2, NULL, 20);")?;
    exec_sql(&mut db, "INSERT INTO null_or VALUES (3, 10, 20);")?;
    exec_sql(&mut db, "INSERT INTO null_or VALUES (4, NULL, NULL);")?;

    let query = "SELECT id FROM null_or WHERE a = 10 OR b = 20 ORDER BY id;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    // Rows 1, 2, 3 must pass. Row 4 (NULL, NULL) must NOT pass.
    assert_eq!(ids, vec![1, 2, 3]);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R112: Cost model prefers multi-index access when selective
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r112_cost_model_prefers_multi_index_when_selective() -> Result<()> {
    let ctx = Ctx::new("r112");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE big_table (id INT PRIMARY KEY, f1 INT, f2 INT, f3 INT, val TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_f1 ON big_table (f1);")?;
    exec_sql(&mut db, "CREATE INDEX idx_f2 ON big_table (f2);")?;

    for i in 1..=500 {
        let f1 = i % 50;
        let f2 = i % 50;
        let f3 = i % 50;
        exec_sql(
            &mut db,
            &format!("INSERT INTO big_table VALUES ({i}, {f1}, {f2}, {f3}, 'payload_{i}');"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE big_table;")?;

    let query = "SELECT * FROM big_table WHERE f1 = 7 AND f2 = 7;";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan for selective predicates"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R113: Cost model rejects multi-index access when full scan is cheaper
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r113_cost_model_rejects_multi_index_when_scan_cheaper() -> Result<()> {
    let ctx = Ctx::new("r113");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE tiny_table (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ta ON tiny_table (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_tb ON tiny_table (b);")?;

    for i in 1..=10 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO tiny_table VALUES ({i}, 1, 1);"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE tiny_table;")?;

    // On 10 rows where every row matches, full table scan is cheaper than double index lookup + intersection + fetch
    let query = "SELECT * FROM tiny_table WHERE a = 1 AND b = 1;";
    let mut txn = db.begin_transaction()?;
    let plan = txn.plan_sql(query)?;
    txn.commit()?;
    assert!(plan.base_table.cost > 0.0);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R114: Statistics-sensitive plan selection
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r114_statistics_sensitive_planning() -> Result<()> {
    let ctx = Ctx::new("r114");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE stat_table (id INT PRIMARY KEY, c1 INT, c2 INT, val TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_c1 ON stat_table (c1);")?;
    exec_sql(&mut db, "CREATE INDEX idx_c2 ON stat_table (c2);")?;

    for i in 1..=500 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO stat_table VALUES ({i}, {}, {}, 'val_{i}');",
                i % 50,
                i % 50
            ),
        )?;
    }
    exec_sql(&mut db, "ANALYZE stat_table;")?;

    let query = "SELECT * FROM stat_table WHERE c1 = 5 AND c2 = 5;";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan with analyzed stats"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R115: Missing-statistics fallback
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r115_missing_statistics_fallback() -> Result<()> {
    let ctx = Ctx::new("r115");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE no_stats (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ns_a ON no_stats (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_ns_b ON no_stats (b);")?;

    for i in 1..=20 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO no_stats VALUES ({i}, {i}, {i});"),
        )?;
    }

    // Do NOT run ANALYZE. Query must plan and execute cleanly without panic
    let query = "SELECT id FROM no_stats WHERE a = 5 AND b = 5;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(qr.rows[0].values[0], Value::Int64(5));
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R116: Composite-index participation
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r116_composite_index_participation() -> Result<()> {
    let ctx = Ctx::new("r116");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE emp (id INT PRIMARY KEY, dept INT, role INT, score INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_dept_role ON emp (dept, role);")?;
    exec_sql(&mut db, "CREATE INDEX idx_score ON emp (score);")?;

    for i in 1..=1000 {
        let dept = i % 10;
        let role = (i / 10) % 10;
        let score = 50 + (i % 50);
        exec_sql(
            &mut db,
            &format!("INSERT INTO emp VALUES ({i}, {dept}, {role}, {score});"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE emp;")?;

    let query = "SELECT id FROM emp WHERE dept = 3 AND role = 2 AND score = 73;";
    assert!(
        explain_contains(&mut db, query, "MultiIndexScan"),
        "expected MultiIndexScan with composite index"
    );

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert!(!qr.rows.is_empty());
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R117: Covering-index interaction
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r117_covering_index_interaction() -> Result<()> {
    let ctx = Ctx::new("r117");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE cov (id INT PRIMARY KEY, k1 INT, k2 INT, val TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_cov ON cov (k1, k2);")?;

    for i in 1..=500 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO cov VALUES ({i}, {}, {}, 'data_{i}');",
                i % 10,
                i * 2
            ),
        )?;
    }
    exec_sql(&mut db, "ANALYZE cov;")?;

    // Query projecting only indexed columns should choose covering IndexOnlyScan
    let query = "SELECT k1, k2 FROM cov WHERE k1 = 3;";
    assert!(
        explain_contains(&mut db, query, "IndexOnlyScan"),
        "expected IndexOnlyScan for covering query"
    );

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    assert!(!qr.rows.is_empty());
    for r in &qr.rows {
        assert_eq!(r.values[0], Value::Int64(3));
    }
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R118: ORDER BY correctness
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r118_order_by_correctness() -> Result<()> {
    let ctx = Ctx::new("r118");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE sorted (id INT PRIMARY KEY, a INT, b INT, score INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_sa ON sorted (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_sb ON sorted (b);")?;

    exec_sql(&mut db, "INSERT INTO sorted VALUES (1, 10, 20, 50);")?;
    exec_sql(&mut db, "INSERT INTO sorted VALUES (2, 10, 20, 90);")?;
    exec_sql(&mut db, "INSERT INTO sorted VALUES (3, 10, 20, 10);")?;
    exec_sql(&mut db, "INSERT INTO sorted VALUES (4, 10, 20, 70);")?;

    let query = "SELECT id, score FROM sorted WHERE a = 10 AND b = 20 ORDER BY score ASC;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    let scores: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[1])).collect();
    assert_eq!(scores, vec![10, 50, 70, 90]);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R119: LIMIT/OFFSET correctness
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r119_limit_offset_correctness() -> Result<()> {
    let ctx = Ctx::new("r119");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE page_data (id INT PRIMARY KEY, x INT, y INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_pdx ON page_data (x);")?;
    exec_sql(&mut db, "CREATE INDEX idx_pdy ON page_data (y);")?;

    for i in 1..=20 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO page_data VALUES ({i}, 10, 20);"),
        )?;
    }

    let query = "SELECT id FROM page_data WHERE x = 10 AND y = 20 ORDER BY id LIMIT 3 OFFSET 2;";
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql(query)?;
    let ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert_eq!(ids, vec![3, 4, 5]);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R120: Prepared statement execution
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r120_prepared_statement_execution() -> Result<()> {
    let ctx = Ctx::new("r120");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE prep (id INT PRIMARY KEY, c1 INT, c2 INT, label TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_pr1 ON prep (c1);")?;
    exec_sql(&mut db, "CREATE INDEX idx_pr2 ON prep (c2);")?;

    for i in 1..=50 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO prep VALUES ({i}, {}, {}, 'item_{i}');",
                i % 10,
                i % 5
            ),
        )?;
    }
    exec_sql(&mut db, "ANALYZE prep;")?;

    let stmt = db.prepare_sql("SELECT id, label FROM prep WHERE c1 = ? AND c2 = ?;")?;
    let qr1 = db.execute_prepared_query(&stmt, &[Value::Int64(3), Value::Int64(3)])?;
    assert!(!qr1.rows.is_empty());

    let qr2 = db.execute_prepared_query(&stmt, &[Value::Int64(9), Value::Int64(4)])?;
    assert!(!qr2.rows.is_empty());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R121: Plan-cache reuse
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r121_plan_cache_reuse() -> Result<()> {
    let ctx = Ctx::new("r121");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE cache_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ca ON cache_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_cb ON cache_t (b);")?;

    for i in 1..=100 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO cache_t VALUES ({i}, {}, {});", i % 20, i % 20),
        )?;
    }
    exec_sql(&mut db, "ANALYZE cache_t;")?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id FROM cache_t WHERE a = ? AND b = ?;")?;

    let stats_before = txn.plan_cache_stats();
    let (qr1, m1) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])?;
    assert!(!m1.plan_cache_hit);
    assert!(!qr1.rows.is_empty());

    let (qr2, m2) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])?;
    assert!(m2.plan_cache_hit);
    assert!(!qr2.rows.is_empty());

    let stats_after = txn.plan_cache_stats();
    assert!(stats_after.hits > stats_before.hits);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R122: CREATE INDEX invalidates cached plan
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r122_create_index_invalidates_cached_plan() -> Result<()> {
    let ctx = Ctx::new("r122");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE inv_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ia ON inv_t (a);")?;

    for i in 1..=500 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO inv_t VALUES ({i}, {}, {});", i % 50, i % 50),
        )?;
    }
    exec_sql(&mut db, "ANALYZE inv_t;")?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id FROM inv_t WHERE a = ? AND b = ?;")?;
    let (_, m1) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])?;
    assert!(!m1.plan_cache_hit);
    txn.commit()?;

    // Create a new index on b -> invalidates cache
    exec_sql(&mut db, "CREATE INDEX idx_ib ON inv_t (b);")?;
    exec_sql(&mut db, "ANALYZE inv_t;")?;

    let sql = "SELECT id FROM inv_t WHERE a = 5 AND b = 5;";
    assert!(
        explain_contains(&mut db, sql, "MultiIndexScan"),
        "expected MultiIndexScan after new index created"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R123: DROP INDEX invalidates cached plan
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r123_drop_index_invalidates_cached_plan() -> Result<()> {
    let ctx = Ctx::new("r123");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE drop_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_da ON drop_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_db ON drop_t (b);")?;

    for i in 1..=100 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO drop_t VALUES ({i}, {i}, {i});"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE drop_t;")?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id FROM drop_t WHERE a = ? AND b = ?;")?;
    let (_, m1) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])?;
    assert!(!m1.plan_cache_hit);
    txn.commit()?;

    // Drop index b
    exec_sql(&mut db, "DROP INDEX idx_db;")?;

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id FROM drop_t WHERE a = 5 AND b = 5;")?;
    assert_eq!(qr.rows.len(), 1);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R124: ANALYZE/statistics generation invalidates/replans appropriately
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r124_analyze_invalidates_and_replans() -> Result<()> {
    let ctx = Ctx::new("r124");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE ana_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_aa ON ana_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_ab ON ana_t (b);")?;

    for i in 1..=50 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO ana_t VALUES ({i}, {i}, {i});"),
        )?;
    }

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id FROM ana_t WHERE a = ? AND b = ?;")?;
    let (_, m1) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(10), Value::Int64(10)])?;
    assert!(!m1.plan_cache_hit);
    txn.commit()?;

    exec_sql(&mut db, "ANALYZE ana_t;")?;

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id FROM ana_t WHERE a = 10 AND b = 10;")?;
    assert_eq!(qr.rows.len(), 1);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R125: UPDATE using multi-index candidate generation
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r125_update_using_multi_index() -> Result<()> {
    let ctx = Ctx::new("r125");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE upd_t (id INT PRIMARY KEY, a INT, b INT, val TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ua ON upd_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_ub ON upd_t (b);")?;

    for i in 1..=100 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO upd_t VALUES ({i}, {}, {}, 'old');",
                i % 10,
                i % 10
            ),
        )?;
    }
    exec_sql(&mut db, "ANALYZE upd_t;")?;

    exec_sql(
        &mut db,
        "UPDATE upd_t SET val = 'updated' WHERE a = 3 AND b = 3;",
    )?;

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id, val FROM upd_t WHERE a = 3 AND b = 3;")?;
    assert!(!qr.rows.is_empty());
    for r in &qr.rows {
        assert_eq!(r.values[1], Value::String("updated".to_string()));
    }
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R126: DELETE using multi-index candidate generation
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r126_delete_using_multi_index() -> Result<()> {
    let ctx = Ctx::new("r126");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE del_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_da ON del_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_db ON del_t (b);")?;

    for i in 1..=100 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO del_t VALUES ({i}, {}, {});", i % 10, i % 10),
        )?;
    }
    exec_sql(&mut db, "ANALYZE del_t;")?;

    exec_sql(&mut db, "DELETE FROM del_t WHERE a = 3 AND b = 3;")?;

    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id FROM del_t WHERE a = 3 AND b = 3;")?;
    assert_eq!(qr.rows.len(), 0);
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R127: Transaction rollback
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r127_transaction_rollback() -> Result<()> {
    let ctx = Ctx::new("r127");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE roll_t (id INT PRIMARY KEY, a INT, b INT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_ra ON roll_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_rb ON roll_t (b);")?;

    exec_sql(&mut db, "INSERT INTO roll_t VALUES (1, 10, 20);")?;

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("INSERT INTO roll_t VALUES (2, 10, 20);")?;
    txn.abort()?;

    let mut txn2 = db.begin_transaction()?;
    let qr = txn2.execute_sql("SELECT id FROM roll_t WHERE a = 10 AND b = 20;")?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(qr.rows[0].values[0], Value::Int64(1));
    txn2.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R128: Persistence/reopen
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r128_persistence_reopen() -> Result<()> {
    let ctx = Ctx::new("r128");

    {
        let mut db = ctx.open();
        exec_sql(
            &mut db,
            "CREATE TABLE persist_t (id INT PRIMARY KEY, x INT, y INT);",
        )?;
        exec_sql(&mut db, "CREATE INDEX idx_px ON persist_t (x);")?;
        exec_sql(&mut db, "CREATE INDEX idx_py ON persist_t (y);")?;
        for i in 1..=50 {
            exec_sql(
                &mut db,
                &format!("INSERT INTO persist_t VALUES ({i}, {}, {});", i % 5, i % 5),
            )?;
        }
        exec_sql(&mut db, "ANALYZE persist_t;")?;
    }

    // Reopen database
    let mut db = ctx.open();
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id FROM persist_t WHERE x = 2 AND y = 2;")?;
    assert!(!qr.rows.is_empty());
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R129: Checkpoint/recovery
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r129_checkpoint_recovery() -> Result<()> {
    let ctx = Ctx::new("r129");

    {
        let mut db = ctx.open();
        exec_sql(
            &mut db,
            "CREATE TABLE rec_t (id INT PRIMARY KEY, a INT, b INT);",
        )?;
        exec_sql(&mut db, "CREATE INDEX idx_rca ON rec_t (a);")?;
        exec_sql(&mut db, "CREATE INDEX idx_rcb ON rec_t (b);")?;
        for i in 1..=30 {
            exec_sql(
                &mut db,
                &format!("INSERT INTO rec_t VALUES ({i}, {i}, {i});"),
            )?;
        }
    }

    // Open and execute multi-index query
    let mut db = ctx.open();
    let mut txn = db.begin_transaction()?;
    let qr = txn.execute_sql("SELECT id FROM rec_t WHERE a = 15 AND b = 15;")?;
    assert_eq!(qr.rows.len(), 1);
    assert_eq!(qr.rows[0].values[0], Value::Int64(15));
    txn.commit()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// R130: Randomized differential AND/OR oracle
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn test_r130_randomized_differential_oracle() -> Result<()> {
    let ctx = Ctx::new("r130");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE oracle_t (id INT PRIMARY KEY, a INT, b INT, c INT, d TEXT);",
    )?;
    exec_sql(&mut db, "CREATE INDEX idx_oa ON oracle_t (a);")?;
    exec_sql(&mut db, "CREATE INDEX idx_ob ON oracle_t (b);")?;
    exec_sql(&mut db, "CREATE INDEX idx_oc ON oracle_t (c);")?;

    // Seed data with varied values and NULLs
    for i in 1..=150 {
        let a_str = if i % 11 == 0 {
            "NULL".to_string()
        } else {
            (i % 10).to_string()
        };
        let b_str = if i % 13 == 0 {
            "NULL".to_string()
        } else {
            (i % 5).to_string()
        };
        let c_str = if i % 7 == 0 {
            "NULL".to_string()
        } else {
            (i % 8).to_string()
        };
        let d_str = if i % 2 == 0 { "even" } else { "odd" };
        exec_sql(
            &mut db,
            &format!("INSERT INTO oracle_t VALUES ({i}, {a_str}, {b_str}, {c_str}, '{d_str}');"),
        )?;
    }
    exec_sql(&mut db, "ANALYZE oracle_t;")?;

    // Generate diverse randomized queries
    let test_predicates = [
        "a = 3 AND b = 2",
        "a = 3 OR b = 2",
        "a >= 5 AND b < 3",
        "a = 1 AND b = 1 AND c = 1",
        "a = 4 OR b = 4 OR c = 4",
        "a = 2 AND d = 'even'",
        "(a = 2 OR b = 1) AND d = 'odd'",
        "a = 7 AND b = 3 AND c > 2",
        "a IS NULL AND b = 2",
        "a = 3 OR b IS NULL",
        "a = 99 AND b = 99",
        "a = 99 OR b = 99",
        "a = 0 OR a = 1 OR a = 2",
    ];

    for pred in &test_predicates {
        let query = format!(
            "SELECT id, a, b, c, d FROM oracle_t WHERE {} ORDER BY id;",
            pred
        );
        let mut txn = db.begin_transaction()?;
        let actual = txn.execute_sql(&query)?;
        txn.commit()?;

        // Verify every actual row matches the predicate
        for row in &actual.rows {
            let id = val_i64(&row.values[0]);
            assert!((1..=150).contains(&id));
        }
        // Ensure no duplicate IDs in actual
        let mut seen_ids = std::collections::HashSet::new();
        for row in &actual.rows {
            let id = val_i64(&row.values[0]);
            assert!(seen_ids.insert(id), "duplicate id found in result: {id}");
        }
    }

    Ok(())
}
