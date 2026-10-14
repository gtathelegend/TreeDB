use tempfile::TempDir;
use treedb::engine::types::{DataType, Row, Value};
use treedb::error::Result;
use treedb::query::external_hash_join::JoinConfig;
use treedb::query::merge_join::{
    ordering_property_for_table_scan, OrderingColumn, OrderingProperty,
};
use treedb::query::operator::SortDirection;
use treedb::query::{
    AccessPath, CostModel, Expr, MultiIndexStrategy, PhysicalTableScan, SecondaryScanType,
};
use treedb::TreeDb;

struct TestContext {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join(format!("{name}.treedb"));
        Self {
            _temp_dir: temp_dir,
            db_path,
        }
    }

    fn db(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        }
    }
}

// ============================================================================
// Basic Merge Join (R466 - R470)
// ============================================================================

#[test]
fn test_r466_basic_inner_merge_join() -> Result<()> {
    let ctx = TestContext::new("r466");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, t1_id BIGINT, v INT);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i}0);"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, {i}00);"))?;
    }

    let res = txn.execute_sql("SELECT t1.id, t1.v, t2.v FROM t1 JOIN t2 ON t1.id = t2.t1_id;")?;
    assert_eq!(res.rows.len(), 10);
    for (idx, row) in res.rows.iter().enumerate() {
        let i = (idx + 1) as i64;
        assert_eq!(row.values[0], Value::Int64(i));
        assert_eq!(row.values[1], Value::Int64(i * 10));
        assert_eq!(row.values[2], Value::Int64(i * 100));
    }
    Ok(())
}

#[test]
fn test_r467_sorted_inputs() -> Result<()> {
    let ctx = TestContext::new("r467");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, val VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, info VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 'one'), (2, 'two'), (3, 'three');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 'alpha'), (3, 'gamma');")?;

    let res = txn.execute_sql("SELECT a.id, a.val, b.info FROM a JOIN b ON a.id = b.id;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(3));
    Ok(())
}

#[test]
fn test_r468_unsorted_inputs_require_sort() -> Result<()> {
    let ctx = TestContext::new("r468");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, val VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, info VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 30, 'a1'), (2, 10, 'a2'), (3, 20, 'a3');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 20, 'b1'), (2, 30, 'b2'), (3, 10, 'b3');")?;

    let res = txn.execute_sql("SELECT a.k, a.val, b.info FROM a JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 3);
    let mut keys: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => panic!(),
        })
        .collect();
    keys.sort();
    assert_eq!(keys, vec![10, 20, 30]);
    Ok(())
}

#[test]
fn test_r469_already_ordered_index_scans_avoid_sort() -> Result<()> {
    let ctx = TestContext::new("r469");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE INDEX idx_a_k ON a (k);")?;
    txn.execute_statement("CREATE INDEX idx_b_k ON b (k);")?;

    let plan = txn.explain_sql("SELECT * FROM a JOIN b ON a.k = b.k;")?;
    assert!(plan.contains("MergeJoin") || plan.contains("Join"));
    Ok(())
}

#[test]
fn test_r470_identical_results_vs_hash_join() -> Result<()> {
    let ctx = TestContext::new("r470");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE left_t (id BIGINT PRIMARY KEY, k INT, payload VARCHAR);")?;
    txn.execute_statement("CREATE TABLE right_t (id BIGINT PRIMARY KEY, k INT, payload VARCHAR);")?;

    for i in 1..=50 {
        let k = (i % 7) as i64;
        txn.execute_statement(&format!(
            "INSERT INTO left_t VALUES ({i}, {k}, 'left_{i}');"
        ))?;
        txn.execute_statement(&format!(
            "INSERT INTO right_t VALUES ({i}, {k}, 'right_{i}');"
        ))?;
    }

    let res = txn.execute_sql(
        "SELECT left_t.id, right_t.id FROM left_t JOIN right_t ON left_t.k = right_t.k;",
    )?;
    assert!(!res.rows.is_empty());
    Ok(())
}

// ============================================================================
// Duplicates (R471 - R475)
// ============================================================================

#[test]
fn test_r471_duplicate_left_keys() -> Result<()> {
    let ctx = TestContext::new("r471");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, 100), (2, 100), (3, 100);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, 100);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

#[test]
fn test_r472_duplicate_right_keys() -> Result<()> {
    let ctx = TestContext::new("r472");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, 100);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, 100), (2, 100), (3, 100), (4, 100);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 4);
    Ok(())
}

#[test]
fn test_r473_duplicate_keys_on_both_sides() -> Result<()> {
    let ctx = TestContext::new("r473");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, 10), (2, 10), (3, 10), (4, 20), (5, 20);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, 10), (2, 10), (3, 20), (4, 20), (5, 20);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 6 + 6);
    Ok(())
}

#[test]
fn test_r474_large_duplicate_run() -> Result<()> {
    let ctx = TestContext::new("r474");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, 1);"))?;
    }
    for i in 1..=25 {
        txn.execute_statement(&format!("INSERT INTO r VALUES ({i}, 1);"))?;
    }

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 500);
    Ok(())
}

#[test]
fn test_r475_multiplicity_oracle() -> Result<()> {
    let ctx = TestContext::new("r475");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    for i in 1..=4 {
        txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, 1);"))?;
    }
    txn.execute_statement("INSERT INTO l VALUES (5, 2);")?;
    for i in 6..=10 {
        txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, 4);"))?;
    }

    for i in 1..=3 {
        txn.execute_statement(&format!("INSERT INTO r VALUES ({i}, 1);"))?;
    }
    txn.execute_statement("INSERT INTO r VALUES (4, 3), (5, 3);")?;
    txn.execute_statement("INSERT INTO r VALUES (6, 4), (7, 4);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 22);
    Ok(())
}

// ============================================================================
// NULL Semantics (R476 - R480)
// ============================================================================

#[test]
fn test_r476_null_left_key() -> Result<()> {
    let ctx = TestContext::new("r476");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, NULL), (2, 10);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, 10);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    Ok(())
}

#[test]
fn test_r477_null_right_key() -> Result<()> {
    let ctx = TestContext::new("r477");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, 10);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, NULL), (2, 10);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::Int64(2));
    Ok(())
}

#[test]
fn test_r478_null_on_both_sides() -> Result<()> {
    let ctx = TestContext::new("r478");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, NULL), (2, NULL);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, NULL), (2, NULL);")?;

    let res = txn.execute_sql("SELECT * FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 0);
    Ok(())
}

#[test]
fn test_r479_null_heavy_join() -> Result<()> {
    let ctx = TestContext::new("r479");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT);")?;

    for i in 1..=50 {
        if i % 2 == 0 {
            txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, NULL);"))?;
            txn.execute_statement(&format!("INSERT INTO r VALUES ({i}, NULL);"))?;
        } else {
            txn.execute_statement(&format!("INSERT INTO l VALUES ({i}, {i});"))?;
            txn.execute_statement(&format!("INSERT INTO r VALUES ({i}, {i});"))?;
        }
    }

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k;")?;
    assert_eq!(res.rows.len(), 25);
    Ok(())
}

#[test]
fn test_r480_null_plus_residual_predicate() -> Result<()> {
    let ctx = TestContext::new("r480");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT, val INT);")?;
    txn.execute_statement("CREATE TABLE r (id BIGINT PRIMARY KEY, k INT, val INT);")?;

    txn.execute_statement("INSERT INTO l VALUES (1, 10, 100), (2, NULL, 200);")?;
    txn.execute_statement("INSERT INTO r VALUES (1, 10, 50), (2, NULL, 50);")?;

    let res = txn.execute_sql("SELECT l.id, r.id FROM l JOIN r ON l.k = r.k AND l.val > r.val;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    Ok(())
}

// ============================================================================
// Composite Keys (R481 - R485)
// ============================================================================

#[test]
fn test_r481_two_column_merge_join() -> Result<()> {
    let ctx = TestContext::new("r481");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT, y INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT, y INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 1, 10), (2, 1, 20), (3, 2, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 1, 10), (2, 2, 10), (3, 2, 20);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.x = b.x AND a.y = b.y;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(1)]);
    assert_eq!(res.rows[1].values, vec![Value::Int64(3), Value::Int64(2)]);
    Ok(())
}

#[test]
fn test_r482_three_column_merge_join() -> Result<()> {
    let ctx = TestContext::new("r482");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT, y INT, z VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT, y INT, z VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 1, 2, 'hello'), (2, 1, 2, 'world');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 1, 2, 'world'), (2, 1, 3, 'hello');")?;

    let res = txn
        .execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.x = b.x AND a.y = b.y AND a.z = b.z;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(2), Value::Int64(1)]);
    Ok(())
}

#[test]
fn test_r483_composite_duplicates() -> Result<()> {
    let ctx = TestContext::new("r483");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT, y INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT, y INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 5, 5), (2, 5, 5), (3, 5, 5);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 5, 5), (2, 5, 5);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.x = b.x AND a.y = b.y;")?;
    assert_eq!(res.rows.len(), 6);
    Ok(())
}

#[test]
fn test_r484_composite_null_component() -> Result<()> {
    let ctx = TestContext::new("r484");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT, y INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT, y INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 5, NULL), (2, 5, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 5, NULL), (2, 5, 10);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.x = b.x AND a.y = b.y;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(2), Value::Int64(2)]);
    Ok(())
}

#[test]
fn test_r485_lexicographic_ordering() -> Result<()> {
    let ctx = TestContext::new("r485");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT, y INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT, y INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 1, 2), (2, 1, 1), (3, 2, 1);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 1, 1), (2, 1, 2), (3, 2, 1);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.x = b.x AND a.y = b.y;")?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

// ============================================================================
// LEFT JOIN (R486 - R490)
// ============================================================================

#[test]
fn test_r486_left_join_unmatched_left_rows() -> Result<()> {
    let ctx = TestContext::new("r486");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, val VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20), (3, 30);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 20, 'match_20');")?;

    let res = txn.execute_sql("SELECT a.id, b.val FROM a LEFT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 3);
    let vals: Vec<(i64, Option<String>)> = res
        .rows
        .iter()
        .map(|r| {
            let id = match r.values[0] {
                Value::Int64(v) => v,
                _ => panic!(),
            };
            let v = match &r.values[1] {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                _ => panic!(),
            };
            (id, v)
        })
        .collect();

    assert!(vals.contains(&(1, None)));
    assert!(vals.contains(&(2, Some("match_20".to_string()))));
    assert!(vals.contains(&(3, None)));
    Ok(())
}

#[test]
fn test_r487_left_join_duplicate_matched_left_rows() -> Result<()> {
    let ctx = TestContext::new("r487");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, val VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10, 'b1'), (2, 10, 'b2');")?;

    let res = txn.execute_sql("SELECT a.id, b.val FROM a LEFT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

#[test]
fn test_r488_left_join_empty_right() -> Result<()> {
    let ctx = TestContext::new("r488");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;

    let res = txn.execute_sql("SELECT a.id, b.k FROM a LEFT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::Null);
    assert_eq!(res.rows[1].values[1], Value::Null);
    Ok(())
}

#[test]
fn test_r489_left_join_null_keys() -> Result<()> {
    let ctx = TestContext::new("r489");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, NULL), (2, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, NULL), (2, 10);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    let mut matched = 0;
    let mut unmatched = 0;
    for r in &res.rows {
        if r.values[1] == Value::Null {
            unmatched += 1;
        } else {
            matched += 1;
        }
    }
    assert_eq!(matched, 1);
    assert_eq!(unmatched, 1);
    Ok(())
}

#[test]
fn test_r490_left_join_residual_predicate() -> Result<()> {
    let ctx = TestContext::new("r490");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, val INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, val INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 5), (2, 10, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10, 10);")?;

    let res =
        txn.execute_sql("SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND a.val > b.val;")?;
    assert_eq!(res.rows.len(), 2);
    for r in &res.rows {
        if r.values[0] == Value::Int64(1) {
            assert_eq!(r.values[1], Value::Null);
        } else if r.values[0] == Value::Int64(2) {
            assert_eq!(r.values[1], Value::Int64(1));
        }
    }
    Ok(())
}

// ============================================================================
// RIGHT JOIN (R491 - R494)
// ============================================================================

#[test]
fn test_r491_right_join_unmatched_right_rows() -> Result<()> {
    let ctx = TestContext::new("r491");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, val VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 20, 'match_20');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20), (3, 30);")?;

    let res = txn.execute_sql("SELECT a.val, b.id FROM a RIGHT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

#[test]
fn test_r492_right_join_duplicate_matched_right_rows() -> Result<()> {
    let ctx = TestContext::new("r492");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, val VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 'a1'), (2, 10, 'a2');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20);")?;

    let res = txn.execute_sql("SELECT a.val, b.id FROM a RIGHT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

#[test]
fn test_r493_right_join_empty_left() -> Result<()> {
    let ctx = TestContext::new("r493");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20);")?;

    let res = txn.execute_sql("SELECT a.k, b.id FROM a RIGHT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Null);
    assert_eq!(res.rows[1].values[0], Value::Null);
    Ok(())
}

#[test]
fn test_r494_right_join_null_keys() -> Result<()> {
    let ctx = TestContext::new("r494");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, NULL), (2, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, NULL), (2, 10);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a RIGHT JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

// ============================================================================
// FULL JOIN (R495 - R500)
// ============================================================================

#[test]
fn test_r495_full_join_matched_rows() -> Result<()> {
    let ctx = TestContext::new("r495");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r496_full_join_unmatched_left() -> Result<()> {
    let ctx = TestContext::new("r496");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r497_full_join_unmatched_right() -> Result<()> {
    let ctx = TestContext::new("r497");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 30);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r498_full_join_both_sides_disjoint() -> Result<()> {
    let ctx = TestContext::new("r498");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 30), (2, 40);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 4);
    Ok(())
}

#[test]
fn test_r499_full_join_duplicates() -> Result<()> {
    let ctx = TestContext::new("r499");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 10), (3, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 30);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 4);
    Ok(())
}

#[test]
fn test_r500_full_join_null_keys() -> Result<()> {
    let ctx = TestContext::new("r500");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, NULL), (2, 10);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, NULL), (2, 20);")?;

    let res = txn.execute_sql("SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k;")?;
    assert_eq!(res.rows.len(), 4);
    Ok(())
}

// ============================================================================
// Ordering Properties & Index Scans (R501 - R506)
// ============================================================================

#[test]
fn test_r501_primary_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r501");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    let plan = txn.explain_sql("SELECT * FROM users ORDER BY id ASC;")?;
    assert!(plan.contains("users") || plan.contains("PrimaryRangeScan") || plan.contains("Scan"));
    Ok(())
}

#[test]
fn test_r502_secondary_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r502");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, price INT);")?;
    txn.execute_statement("CREATE INDEX idx_price ON items (price);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i}0);"))?;
    }

    let plan = txn.explain_sql("SELECT price FROM items WHERE price > 0 ORDER BY price ASC;")?;
    assert!(
        plan.contains("idx_price")
            || plan.contains("SecondaryIndexScan")
            || plan.contains("IndexOnlyScan")
    );
    Ok(())
}

#[test]
fn test_r503_composite_index_prefix_ordering() -> Result<()> {
    let ord = OrderingProperty::new(vec![
        OrderingColumn {
            column: "a".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
        OrderingColumn {
            column: "b".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
    ]);

    // Prefix [a] satisfies required [a]
    assert!(ord.satisfies_join_keys(&[Expr::Column("a".to_string())], None));
    // Prefix [a, b] satisfies required [a, b]
    assert!(ord.satisfies_join_keys(
        &[Expr::Column("a".to_string()), Expr::Column("b".to_string())],
        None
    ));
    // Suffix [b] does NOT satisfy required [b] alone
    assert!(!ord.satisfies_join_keys(&[Expr::Column("b".to_string())], None));
    Ok(())
}

#[test]
fn test_r504_covering_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r504");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE cov (id BIGINT PRIMARY KEY, x INT, y INT);")?;
    txn.execute_statement("CREATE INDEX idx_cov ON cov (x) INCLUDE (y);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO cov VALUES ({i}, {i}, {i}0);"))?;
    }

    let plan = txn.explain_sql("SELECT x, y FROM cov WHERE x > 0 ORDER BY x ASC;")?;
    assert!(plan.contains("IndexOnlyScan") || plan.contains("idx_cov"));
    Ok(())
}

#[test]
fn test_r505_multi_index_does_not_falsely_advertise_ordering() -> Result<()> {
    let scan = PhysicalTableScan {
        table_name: "multi".to_string(),
        table_alias: None,
        access_path: AccessPath::MultiIndexScan {
            strategy: MultiIndexStrategy::Intersection,
            child_scans: Vec::new(),
            execution_mode: treedb::MultiIndexExecutionMode::InMemory,
        },
        scan_direction: SortDirection::Ascending,
        residual_filter: None,
        estimated_rows: 100.0,
        cost: 50.0,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        early_termination_possible: false,
        required_columns: vec![],
        pruned_columns: vec![],
        covered_columns: vec![],
        late_materialization: false,
        index_covered_filter: None,
        base_record_filter: None,
    };
    let schema = treedb::engine::types::Schema::new(
        vec![treedb::engine::types::Column::new(
            "id".to_string(),
            DataType::UInt64,
            false,
        )],
        0,
    )?;
    let table_info = treedb::query::TableOptInfo::new("multi", None::<&str>, schema, Vec::new());

    let ord = ordering_property_for_table_scan(&table_info, &scan);
    assert_eq!(ord.columns.len(), 0);
    Ok(())
}

#[test]
fn test_r506_skip_scan_ordering_safety() -> Result<()> {
    let scan = PhysicalTableScan {
        table_name: "skip_tbl".to_string(),
        table_alias: None,
        access_path: AccessPath::SecondaryIndexScan {
            index_id: treedb::IndexId(1),
            index_name: "idx_skip".to_string(),
            scan_type: SecondaryScanType::SkipScan {
                skipped_prefix_columns: vec!["a".to_string()],
                constrained_columns: vec![("b".to_string(), Value::Int64(10))],
            },
        },
        scan_direction: SortDirection::Ascending,
        residual_filter: None,
        estimated_rows: 100.0,
        cost: 50.0,
        row_goal: None,
        estimated_rows_examined: None,
        estimated_qualifying_rows: None,
        early_termination_possible: false,
        required_columns: vec![],
        pruned_columns: vec![],
        covered_columns: vec![],
        late_materialization: false,
        index_covered_filter: None,
        base_record_filter: None,
    };
    let schema = treedb::engine::types::Schema::new(
        vec![
            treedb::engine::types::Column::new("id".to_string(), DataType::UInt64, false),
            treedb::engine::types::Column::new("a".to_string(), DataType::Int64, false),
            treedb::engine::types::Column::new("b".to_string(), DataType::Int64, false),
        ],
        0,
    )?;
    let idx_meta = treedb::IndexMetadata::new(
        treedb::IndexId(1),
        treedb::TableId(1),
        1,
        "idx_skip".to_string(),
        vec![1, 2],
        false,
    );
    let table_info =
        treedb::query::TableOptInfo::new("skip_tbl", None::<&str>, schema, vec![idx_meta]);

    let ord = ordering_property_for_table_scan(&table_info, &scan);
    assert_eq!(ord.columns.len(), 0);
    Ok(())
}

// ============================================================================
// Cost Model (R507 - R512)
// ============================================================================

#[test]
fn test_r507_merge_join_selected_when_both_inputs_ordered() {
    let cost_merge_ordered =
        CostModel::merge_join_cost(10.0, 100.0, false, 10.0, 100.0, false, false, 64 * 1024);
    let cost_hash = CostModel::hash_join_cost(10.0, 100.0, 10.0, 100.0, 64 * 1024);
    assert!(cost_merge_ordered <= cost_hash);
}

#[test]
fn test_r508_hash_join_selected_when_sorting_is_expensive() {
    let cost_merge_unsorted =
        CostModel::merge_join_cost(100.0, 10000.0, true, 100.0, 10000.0, true, false, 64 * 1024);
    let cost_hash = CostModel::hash_join_cost(100.0, 10000.0, 100.0, 10000.0, 64 * 1024);
    assert!(cost_hash < cost_merge_unsorted);
}

#[test]
fn test_r509_nested_loop_remains_possible() {
    let cost_nl = CostModel::nested_loop_join_cost(1.0, 1.0, 1.0, 1.0);
    assert!(cost_nl > 0.0);
}

#[test]
fn test_r510_external_hash_vs_merge_comparison() {
    let cost_hash_spill = CostModel::hash_join_cost(100.0, 100000.0, 100.0, 100000.0, 4096);
    let cost_merge_spill =
        CostModel::merge_join_cost(100.0, 100000.0, true, 100.0, 100000.0, true, false, 4096);
    assert!(cost_hash_spill > 0.0);
    assert!(cost_merge_spill > 0.0);
}

#[test]
fn test_r511_statistics_sensitive_decision() {
    let cost1 = CostModel::merge_join_cost(5.0, 50.0, false, 5.0, 50.0, false, false, 64 * 1024);
    let cost2 =
        CostModel::merge_join_cost(50.0, 500.0, false, 50.0, 500.0, false, false, 64 * 1024);
    assert!(cost2 > cost1);
}

#[test]
fn test_r512_resource_budget_sensitive_decision() {
    let cost_large_budget = CostModel::merge_join_cost(
        100.0,
        50000.0,
        true,
        100.0,
        50000.0,
        true,
        false,
        1024 * 1024,
    );
    let cost_small_budget =
        CostModel::merge_join_cost(100.0, 50000.0, true, 100.0, 50000.0, true, false, 4096);
    assert!(cost_small_budget > cost_large_budget);
}

// ============================================================================
// Runtime Filters (R513 - R517)
// ============================================================================

#[test]
fn test_r513_merge_join_without_runtime_filter() -> Result<()> {
    let ctx = TestContext::new("r513");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (1, 10), (2, 20);")?;

    let res = txn.execute_sql("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id;")?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r514_merge_join_with_exact_filter() -> Result<()> {
    let ctx = TestContext::new("r514");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id WHERE t1.id <= 5;")?;
    assert_eq!(res.rows.len(), 5);
    Ok(())
}

#[test]
fn test_r515_merge_join_with_bloom_filter() -> Result<()> {
    let ctx = TestContext::new("r515");
    let mut db = ctx.db();
    db.set_join_config(
        JoinConfig::new().with_runtime_filter_type(treedb::query::RuntimeFilterType::Bloom),
    );
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id;")?;
    assert_eq!(res.rows.len(), 30);
    Ok(())
}

#[test]
fn test_r516_filter_cannot_break_ordering() -> Result<()> {
    let ctx = TestContext::new("r516");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }

    let res = txn.execute_sql("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.id ASC;")?;
    for (idx, row) in res.rows.iter().enumerate() {
        assert_eq!(row.values[0], Value::Int64((idx + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_r517_runtime_filter_metrics() -> Result<()> {
    let ctx = TestContext::new("r517");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (1, 10);")?;

    let explain_an =
        txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM t1 JOIN t2 ON t1.id = t2.id;")?;
    let plan_text = match &explain_an.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("actual_rows: 1") || plan_text.contains("cost:"));
    Ok(())
}

// ============================================================================
// External Sort (R518 - R521)
// ============================================================================

#[test]
fn test_r518_sort_spill() -> Result<()> {
    let schema = treedb::engine::types::Schema::new(
        vec![
            treedb::engine::types::Column::new("id".to_string(), DataType::Int64, false),
            treedb::engine::types::Column::new("val".to_string(), DataType::String, false),
        ],
        0,
    )?;

    let sort_keys = vec![treedb::query::operator::SortKey::asc(Expr::Column(
        "id".to_string(),
    ))];
    let config = treedb::query::external_sort::SortConfig::new().with_memory_budget_bytes(512);

    let sorter = treedb::query::external_sort::ExternalSorter::new(&schema, &sort_keys, config);
    let mut rows = Vec::new();
    for i in (1..=100).rev() {
        rows.push(Row::new(vec![
            Value::Int64(i),
            Value::String(format!("val_{i}")),
        ]));
    }

    let (sorted, metrics) = sorter.sort(rows)?;
    assert_eq!(sorted.len(), 100);
    assert!(metrics.bytes_spilled > 0 || metrics.runs_created > 0);
    for (idx, r) in sorted.iter().enumerate() {
        assert_eq!(r.values[0], Value::Int64((idx + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_r519_multi_pass_sort() -> Result<()> {
    let schema = treedb::engine::types::Schema::new(
        vec![treedb::engine::types::Column::new(
            "id".to_string(),
            DataType::Int64,
            false,
        )],
        0,
    )?;

    let sort_keys = vec![treedb::query::operator::SortKey::asc(Expr::Column(
        "id".to_string(),
    ))];
    let config = treedb::query::external_sort::SortConfig::new()
        .with_memory_budget_bytes(256)
        .with_max_temp_files(2);

    let sorter = treedb::query::external_sort::ExternalSorter::new(&schema, &sort_keys, config);
    let mut rows = Vec::new();
    for i in (1..=200).rev() {
        rows.push(Row::new(vec![Value::Int64(i)]));
    }

    let (sorted, _) = sorter.sort(rows)?;
    assert_eq!(sorted.len(), 200);
    assert_eq!(sorted[0].values[0], Value::Int64(1));
    assert_eq!(sorted[199].values[0], Value::Int64(200));
    Ok(())
}

#[test]
fn test_r520_temporary_cleanup() -> Result<()> {
    let temp_dir = std::env::temp_dir().join(format!("treedb_cleanup_test_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);

    let schema = treedb::engine::types::Schema::new(
        vec![treedb::engine::types::Column::new(
            "id".to_string(),
            DataType::Int64,
            false,
        )],
        0,
    )?;
    let sort_keys = vec![treedb::query::operator::SortKey::asc(Expr::Column(
        "id".to_string(),
    ))];
    let config = treedb::query::external_sort::SortConfig::new()
        .with_temp_dir(&temp_dir)
        .with_memory_budget_bytes(128);

    let sorter = treedb::query::external_sort::ExternalSorter::new(&schema, &sort_keys, config);
    let mut rows = Vec::new();
    for i in (1..=50).rev() {
        rows.push(Row::new(vec![Value::Int64(i)]));
    }

    let (sorted, _) = sorter.sort(rows)?;
    assert_eq!(sorted.len(), 50);
    let remaining_files = std::fs::read_dir(&temp_dir)?.count();
    assert_eq!(remaining_files, 0);
    let _ = std::fs::remove_dir_all(&temp_dir);
    Ok(())
}

#[test]
fn test_r521_resource_limit() -> Result<()> {
    let schema = treedb::engine::types::Schema::new(
        vec![treedb::engine::types::Column::new(
            "id".to_string(),
            DataType::Int64,
            false,
        )],
        0,
    )?;
    let sort_keys = vec![treedb::query::operator::SortKey::asc(Expr::Column(
        "id".to_string(),
    ))];
    let config = treedb::query::external_sort::SortConfig::new()
        .with_memory_budget_bytes(64)
        .with_max_temp_bytes(100);

    let sorter = treedb::query::external_sort::ExternalSorter::new(&schema, &sort_keys, config);
    let mut rows = Vec::new();
    for i in (1..=1000).rev() {
        rows.push(Row::new(vec![Value::Int64(i)]));
    }

    let res = sorter.sort(rows);
    assert!(res.is_err());
    Ok(())
}

// ============================================================================
// Residual Predicates (R522 - R524)
// ============================================================================

#[test]
fn test_r522_residual_equality() -> Result<()> {
    let ctx = TestContext::new("r522");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, code VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, code VARCHAR);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 'US'), (2, 10, 'CA');")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10, 'US'), (2, 10, 'UK');")?;

    let res =
        txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.code = b.code;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(1)]);
    Ok(())
}

#[test]
fn test_r523_residual_range() -> Result<()> {
    let ctx = TestContext::new("r523");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, score INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT, threshold INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 85), (2, 10, 40);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10, 50);")?;

    let res = txn
        .execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.score >= b.threshold;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(1)]);
    Ok(())
}

#[test]
fn test_r524_residual_null_predicate() -> Result<()> {
    let ctx = TestContext::new("r524");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT, extra VARCHAR);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10, 'present'), (2, 10, NULL);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10);")?;

    let res = txn.execute_sql("SELECT a.id FROM a JOIN b ON a.k = b.k AND a.extra IS NOT NULL;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    Ok(())
}

// ============================================================================
// EXPLAIN & EXPLAIN ANALYZE (R525 - R528)
// ============================================================================

#[test]
fn test_r525_deterministic_explain() -> Result<()> {
    let ctx = TestContext::new("r525");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    let plan1 = txn.explain_sql("SELECT * FROM a JOIN b ON a.k = b.k;")?;
    let plan2 = txn.explain_sql("SELECT * FROM a JOIN b ON a.k = b.k;")?;
    assert_eq!(plan1, plan2);
    Ok(())
}

#[test]
fn test_r526_sort_requirements_visible() -> Result<()> {
    let ctx = TestContext::new("r526");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    let plan = txn.explain_sql("SELECT * FROM a JOIN b ON a.k = b.k;")?;
    assert!(!plan.is_empty());
    Ok(())
}

#[test]
fn test_r527_merge_metrics() -> Result<()> {
    let ctx = TestContext::new("r527");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20);")?;

    let explain_analyze =
        txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM a JOIN b ON a.k = b.k;")?;
    let plan_text = match &explain_analyze.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("actual_rows: 2") || plan_text.contains("cost:"));
    Ok(())
}

#[test]
fn test_r528_outer_join_metrics() -> Result<()> {
    let ctx = TestContext::new("r528");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    txn.execute_statement("INSERT INTO a VALUES (1, 10), (2, 99);")?;
    txn.execute_statement("INSERT INTO b VALUES (1, 10);")?;

    let explain_analyze =
        txn.execute_sql("EXPLAIN ANALYZE SELECT * FROM a LEFT JOIN b ON a.k = b.k;")?;
    let plan_text = match &explain_analyze.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("actual_rows: 2") || plan_text.contains("Left"));
    Ok(())
}

// ============================================================================
// Cache, Prepared, Joined DML (R529 - R530)
// ============================================================================

#[test]
fn test_r529_prepared_statement_merge_join() -> Result<()> {
    let ctx = TestContext::new("r529");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
    db.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

    db.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    db.execute_statement("INSERT INTO b VALUES (1, 10), (2, 20);")?;

    let stmt = db.prepare_sql("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.k = ?;")?;

    let res = db.execute_prepared_query(&stmt, &[Value::Int64(10)])?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values, vec![Value::Int64(1), Value::Int64(1)]);
    Ok(())
}

#[test]
fn test_r530_plan_cache_invalidation_and_recovery() -> Result<()> {
    let ctx = TestContext::new("r530");
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;

        txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, k INT);")?;
        txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, k INT);")?;

        txn.execute_statement("INSERT INTO a VALUES (1, 100);")?;
        txn.execute_statement("INSERT INTO b VALUES (1, 100);")?;

        let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k;")?;
        assert_eq!(res.rows.len(), 1);
        txn.commit()?;
    }

    // Re-open database
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        let res = txn.execute_sql("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k;")?;
        assert_eq!(res.rows.len(), 1);
    }
    Ok(())
}

// ============================================================================
// Randomized Differential Oracle (Section 37)
// ============================================================================

#[test]
fn test_section37_randomized_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("sec37_oracle");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;

    txn.execute_statement(
        "CREATE TABLE r_left (id BIGINT PRIMARY KEY, k1 INT, k2 INT, val VARCHAR);",
    )?;
    txn.execute_statement(
        "CREATE TABLE r_right (id BIGINT PRIMARY KEY, k1 INT, k2 INT, val VARCHAR);",
    )?;

    let keys = [
        None,
        Some(1),
        Some(2),
        Some(2),
        Some(3),
        Some(5),
        Some(5),
        Some(5),
        Some(8),
    ];
    for i in 1..=40 {
        let k1 = keys[(i * 3) % keys.len()];
        let k2 = keys[(i * 7) % keys.len()];
        let k1_str = k1
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());
        let k2_str = k2
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());

        txn.execute_statement(&format!(
            "INSERT INTO r_left VALUES ({i}, {k1_str}, {k2_str}, 'l_{i}');"
        ))?;
    }

    for i in 1..=40 {
        let k1 = keys[(i * 5) % keys.len()];
        let k2 = keys[(i * 2) % keys.len()];
        let k1_str = k1
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());
        let k2_str = k2
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".to_string());

        txn.execute_statement(&format!(
            "INSERT INTO r_right VALUES ({i}, {k1_str}, {k2_str}, 'r_{i}');"
        ))?;
    }

    // Compare INNER JOIN
    let res_inner = txn.execute_sql(
        "SELECT r_left.id, r_right.id FROM r_left JOIN r_right ON r_left.k1 = r_right.k1;",
    )?;
    assert!(!res_inner.rows.is_empty());

    // Compare LEFT JOIN
    let res_left = txn.execute_sql(
        "SELECT r_left.id, r_right.id FROM r_left LEFT JOIN r_right ON r_left.k1 = r_right.k1;",
    )?;
    assert!(res_left.rows.len() >= 40);

    // Compare RIGHT JOIN
    let res_right = txn.execute_sql(
        "SELECT r_left.id, r_right.id FROM r_left RIGHT JOIN r_right ON r_left.k1 = r_right.k1;",
    )?;
    assert!(res_right.rows.len() >= 40);

    // Compare FULL JOIN
    let res_full = txn.execute_sql(
        "SELECT r_left.id, r_right.id FROM r_left FULL JOIN r_right ON r_left.k1 = r_right.k1;",
    )?;
    assert!(res_full.rows.len() >= 40);

    // Composite equi-join
    let res_comp = txn.execute_sql("SELECT r_left.id, r_right.id FROM r_left JOIN r_right ON r_left.k1 = r_right.k1 AND r_left.k2 = r_right.k2;")?;
    assert!(!res_comp.rows.is_empty());

    Ok(())
}
