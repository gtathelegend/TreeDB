use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_rec_cte_{name}.treedb"));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        Self {
            _dir: dir,
            db_path,
            wal_path,
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

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

#[test]
fn test_recursive_cte_numbers_1_to_10() {
    let ctx = TestContext::new("nums_1_to_10");
    let mut db = ctx.db();

    // Create a dummy seed table since anchor needs a table or literal
    let mut txn = db.begin().unwrap();
    let seed_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", seed_schema).unwrap();
    txn.execute_statement("INSERT INTO seeds (id, val) VALUES (1, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT val FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 10
    )
    SELECT n FROM nums ORDER BY n ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 10);
    for i in 0..10 {
        assert_eq!(res.rows[i].values[0], Value::Int64(i as i64 + 1));
    }
}

#[test]
fn test_recursive_cte_explicit_column_names() {
    let ctx = TestContext::new("explicit_col_names");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("seed", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("init_tab", schema).unwrap();
    txn.execute_statement("INSERT INTO init_tab (id, seed) VALUES (1, 5)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE counter(custom_col) AS (
        SELECT seed FROM init_tab WHERE id = 1
        UNION ALL
        SELECT custom_col + 2 FROM counter WHERE custom_col < 11
    )
    SELECT custom_col FROM counter ORDER BY custom_col ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.schema.columns[0].name, "custom_col");
    let vals: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => panic!("expected int64"),
        })
        .collect();
    assert_eq!(vals, vec![5, 7, 9, 11]);
}

#[test]
fn test_recursive_cte_empty_anchor() {
    let ctx = TestContext::new("empty_anchor");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(vec![Column::new("id", DataType::UInt64, false)], 0).unwrap();
    txn.create_table("empty_tab", schema).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT id FROM empty_tab
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 10
    )
    SELECT n FROM nums;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 0);
}

#[test]
fn test_recursive_cte_empty_recursive_member() {
    let ctx = TestContext::new("empty_rec_member");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", schema).unwrap();
    txn.execute_statement("INSERT INTO seeds (id, val) VALUES (1, 42)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT val FROM seeds
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 0
    )
    SELECT n FROM nums;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(42));
}

#[test]
fn test_recursive_cte_graph_traversal() {
    let ctx = TestContext::new("graph_traversal");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let edges_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("source", DataType::Int64, false),
            Column::new("target", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("edges", edges_schema).unwrap();

    // Graph: 1 -> 2, 2 -> 3, 2 -> 4, 4 -> 5
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (1, 1, 2)")
        .unwrap();
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (2, 2, 3)")
        .unwrap();
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (3, 2, 4)")
        .unwrap();
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (4, 4, 5)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE reachable(node) AS (
        SELECT target FROM edges WHERE source = 1
        UNION ALL
        SELECT e.target
        FROM edges e
        JOIN reachable r ON e.source = r.node
    )
    SELECT node FROM reachable ORDER BY node ASC;";

    let res = txn.execute_sql(sql).unwrap();
    let nodes: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => panic!("expected int64"),
        })
        .collect();
    assert_eq!(nodes, vec![2, 3, 4, 5]);
}

#[test]
fn test_recursive_cte_cycle_with_safety_limits() {
    let ctx = TestContext::new("cycle_safety");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let edges_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("source", DataType::Int64, false),
            Column::new("target", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("edges", edges_schema).unwrap();

    // Cyclic graph: 1 -> 2, 2 -> 3, 3 -> 1
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (1, 1, 2)")
        .unwrap();
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (2, 2, 3)")
        .unwrap();
    txn.execute_statement("INSERT INTO edges (id, source, target) VALUES (3, 3, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE reachable(node) AS (
        SELECT target FROM edges WHERE source = 1
        UNION ALL
        SELECT e.target
        FROM edges e
        JOIN reachable r ON e.source = r.node
    )
    SELECT node FROM reachable;";

    // Since UNION ALL preserves duplicates, cycling without condition must hit iteration/row limit
    let err = txn.execute_sql(sql).unwrap_err();
    match err {
        Error::RecursiveCteIterationLimit {
            cte,
            iterations,
            limit,
        } => {
            assert_eq!(cte, "reachable");
            assert!(iterations > limit);
        }
        Error::RecursiveCteRowLimit { cte, .. } => {
            assert_eq!(cte, "reachable");
        }
        other => panic!("expected recursive limit error, got {other:?}"),
    }
}

#[test]
fn test_recursive_cte_filters_and_aliases() {
    let ctx = TestContext::new("filters_aliases");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seed_table", schema).unwrap();
    txn.execute_statement("INSERT INTO seed_table (id, val) VALUES (1, 10)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE countdown(num) AS (
        SELECT s.val FROM seed_table s WHERE s.id = 1
        UNION ALL
        SELECT c.num - 2 FROM countdown c WHERE c.num > 0
    )
    SELECT x.num FROM countdown x WHERE x.num <= 6 ORDER BY x.num ASC;";

    let res = txn.execute_sql(sql).unwrap();
    let nums: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => panic!("expected int64"),
        })
        .collect();
    assert_eq!(nums, vec![0, 2, 4, 6]);
}

#[test]
fn test_recursive_cte_preceding_ordinary_cte() {
    let ctx = TestContext::new("preceding_cte");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("active", DataType::Boolean, false),
            Column::new("start_val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("configs", schema).unwrap();
    txn.execute_statement("INSERT INTO configs (id, active, start_val) VALUES (1, true, 100)")
        .unwrap();
    txn.execute_statement("INSERT INTO configs (id, active, start_val) VALUES (2, false, 999)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE
        active_config AS (
            SELECT start_val FROM configs WHERE active = true
        ),
        stepper(v) AS (
            SELECT start_val FROM active_config
            UNION ALL
            SELECT v + 10 FROM stepper WHERE v < 130
        )
    SELECT v FROM stepper ORDER BY v ASC;";

    let res = txn.execute_sql(sql).unwrap();
    let vals: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => panic!("expected int64"),
        })
        .collect();
    assert_eq!(vals, vec![100, 110, 120, 130]);
}

#[test]
fn test_recursive_cte_with_subqueries() {
    let ctx = TestContext::new("subqueries");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 1)")
        .unwrap();

    let l_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("max_v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("limits", l_schema).unwrap();
    txn.execute_statement("INSERT INTO limits (id, max_v) VALUES (1, 5)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < (SELECT max_v FROM limits WHERE id = 1)
    )
    SELECT n FROM nums ORDER BY n ASC;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 5);
    for i in 0..5 {
        assert_eq!(res.rows[i].values[0], Value::Int64(i as i64 + 1));
    }
}

#[test]
fn test_recursive_cte_final_order_by_limit() {
    let ctx = TestContext::new("order_by_limit");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 100
    )
    SELECT n FROM nums
    ORDER BY n DESC
    LIMIT 3 OFFSET 2;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(98));
    assert_eq!(res.rows[1].values[0], Value::Int64(97));
    assert_eq!(res.rows[2].values[0], Value::Int64(96));
}

#[test]
fn test_recursive_cte_final_window_function() {
    let ctx = TestContext::new("final_window");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 5
    )
    SELECT
        n,
        ROW_NUMBER() OVER (ORDER BY n ASC) AS rn
    FROM nums;";

    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 5);
    for i in 0..5 {
        assert_eq!(res.rows[i].values[0], Value::Int64(i as i64 + 1));
        assert_eq!(res.rows[i].values[1], Value::UInt64(i as u64 + 1));
    }
}

#[test]
fn test_recursive_cte_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_visibility");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.commit().unwrap();

    // 1. Transaction inserting row and querying
    let mut txn1 = db.begin().unwrap();
    txn1.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 10)")
        .unwrap();

    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 13
    )
    SELECT n FROM nums ORDER BY n ASC;";

    let res = txn1.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[0], Value::Int64(10));
    assert_eq!(res.rows[3].values[0], Value::Int64(13));

    // Abort txn1
    txn1.abort().unwrap();

    // 2. New transaction should see empty table
    let mut txn2 = db.begin().unwrap();
    let res2 = txn2.execute_sql(sql).unwrap();
    assert_eq!(res2.rows.len(), 0);
    txn2.commit().unwrap();
}

#[test]
fn test_recursive_cte_persistence_across_reopen() {
    let ctx = TestContext::new("reopen");
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        let s_schema = Schema::new(
            vec![
                Column::new("id", DataType::UInt64, false),
                Column::new("v", DataType::Int64, false),
            ],
            0,
        )
        .unwrap();
        txn.create_table("seeds", s_schema).unwrap();
        txn.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 1)")
            .unwrap();
        txn.commit().unwrap();
    }

    // Reopen database
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        let sql = "WITH RECURSIVE nums(n) AS (
            SELECT v FROM seeds WHERE id = 1
            UNION ALL
            SELECT n + 1 FROM nums WHERE n < 4
        )
        SELECT n FROM nums ORDER BY n ASC;";

        let res = txn.execute_sql(sql).unwrap();
        assert_eq!(res.rows.len(), 4);
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[3].values[0], Value::Int64(4));
        txn.commit().unwrap();
    }
}

#[test]
fn test_recursive_cte_duplicate_preservation() {
    let ctx = TestContext::new("duplicate_preservation");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    // Anchor has duplicates
    txn.execute_statement("INSERT INTO seeds (id, v) VALUES (1, 1)")
        .unwrap();
    txn.execute_statement("INSERT INTO seeds (id, v) VALUES (2, 1)")
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 3
    )
    SELECT n FROM nums ORDER BY n ASC;";

    let res = txn.execute_sql(sql).unwrap();
    // Iteration 0: [1, 1]
    // Iteration 1: [2, 2]
    // Iteration 2: [3, 3]
    assert_eq!(res.rows.len(), 6);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(1));
    assert_eq!(res.rows[2].values[0], Value::Int64(2));
    assert_eq!(res.rows[3].values[0], Value::Int64(2));
    assert_eq!(res.rows[4].values[0], Value::Int64(3));
    assert_eq!(res.rows[5].values[0], Value::Int64(3));
}

#[test]
fn test_recursive_cte_schema_and_column_count_mismatch() {
    let ctx = TestContext::new("schema_mismatch");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("people", schema).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();

    // 1. Column count mismatch: anchor has 2 columns, recursive has 1 column
    let sql_col_count = "WITH RECURSIVE r AS (
        SELECT id, name FROM people
        UNION ALL
        SELECT age FROM people
    )
    SELECT * FROM r;";
    let err = txn.execute_sql(sql_col_count).unwrap_err();
    assert!(matches!(err, Error::RecursiveCteColumnCountMismatch { .. }));

    // 2. Type mismatch: anchor column 0 is UInt64, recursive column 0 is String
    let sql_type = "WITH RECURSIVE r AS (
        SELECT id FROM people
        UNION ALL
        SELECT name FROM people
    )
    SELECT * FROM r;";
    let err = txn.execute_sql(sql_type).unwrap_err();
    assert!(matches!(err, Error::RecursiveCteSchemaMismatch { .. }));
}

#[test]
fn test_recursive_cte_anchor_self_reference_rejected() {
    let ctx = TestContext::new("anchor_self_ref");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT n FROM nums
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 10
    )
    SELECT * FROM nums;";

    let err = txn.execute_sql(sql).unwrap_err();
    assert!(matches!(err, Error::RecursiveCteAnchorSelfReference(name) if name == "nums"));
}

#[test]
fn test_recursive_cte_mutual_recursion_rejected() {
    let ctx = TestContext::new("mutual_recursion");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(vec![Column::new("id", DataType::UInt64, false)], 0).unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE
        a AS (
            SELECT id FROM seeds
            UNION ALL
            SELECT id FROM b
        ),
        b AS (
            SELECT id FROM a
            UNION ALL
            SELECT id FROM seeds
        )
    SELECT * FROM a;";

    let err = txn.execute_sql(sql).unwrap_err();
    assert!(matches!(
        err,
        Error::MutualRecursion(_) | Error::CyclicCteDependency(_)
    ));
}

#[test]
fn test_recursive_cte_unsupported_aggregation_and_window_rejected() {
    let ctx = TestContext::new("unsupported_ops");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();

    // Aggregation in recursive member over recursive relation
    let sql_agg = "WITH RECURSIVE r(c) AS (
        SELECT v FROM seeds
        UNION ALL
        SELECT COUNT(*) FROM r
    )
    SELECT * FROM r;";
    let err = txn.execute_sql(sql_agg).unwrap_err();
    assert!(matches!(err, Error::UnsupportedRecursiveAggregation(_)));

    // Window function in recursive member over recursive relation
    let sql_win = "WITH RECURSIVE r(c, rn) AS (
        SELECT v, 1 FROM seeds
        UNION ALL
        SELECT c + 1, ROW_NUMBER() OVER () FROM r
    )
    SELECT * FROM r;";
    let err = txn.execute_sql(sql_win).unwrap_err();
    assert!(matches!(err, Error::UnsupportedRecursiveWindow(_)));
}

#[test]
fn test_recursive_cte_explain() {
    let ctx = TestContext::new("explain");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("seeds", s_schema).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin().unwrap();
    let sql = "WITH RECURSIVE nums(n) AS (
        SELECT v FROM seeds WHERE id = 1
        UNION ALL
        SELECT n + 1 FROM nums WHERE n < 10
    )
    SELECT n FROM nums;";

    let explain_output = txn.explain_sql(sql).unwrap();
    assert!(explain_output.contains("Recursive CTE nums"));
    assert!(explain_output.contains("Anchor"));
    assert!(explain_output.contains("Recursive Member"));
    assert!(explain_output.contains("Max Iterations: 1000"));
    assert!(explain_output.contains("Max Rows: 1000000"));
}

// -----------------------------------------------------------------------
// Pure-Rust Independent Reference Oracle
// -----------------------------------------------------------------------

struct ReferenceRecursiveConfig {
    anchor: Vec<i64>,
    step: i64,
    bound: i64,
    max_iterations: usize,
    max_rows: usize,
}

fn reference_recursive_evaluator(config: &ReferenceRecursiveConfig) -> Result<Vec<i64>, String> {
    let mut accumulated = config.anchor.clone();
    if accumulated.len() > config.max_rows {
        return Err("RowLimit".to_string());
    }
    let mut working = accumulated.clone();
    let mut iterations = 0;

    while !working.is_empty() {
        iterations += 1;
        if iterations > config.max_iterations {
            return Err("IterationLimit".to_string());
        }

        let mut next_working = Vec::new();
        for &item in &working {
            if item < config.bound {
                next_working.push(item + config.step);
            }
        }

        if next_working.is_empty() {
            break;
        }

        if accumulated.len() + next_working.len() > config.max_rows {
            return Err("RowLimit".to_string());
        }

        accumulated.extend(next_working.clone());
        working = next_working;
    }

    Ok(accumulated)
}

#[test]
fn test_randomized_recursive_oracle() {
    let ctx = TestContext::new("oracle");
    let mut db = ctx.db();

    let mut txn = db.begin().unwrap();
    let s_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("v", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("oracle_seeds", s_schema).unwrap();
    txn.commit().unwrap();

    let test_cases = [
        ReferenceRecursiveConfig {
            anchor: vec![],
            step: 1,
            bound: 10,
            max_iterations: 1000,
            max_rows: 1_000_000,
        },
        ReferenceRecursiveConfig {
            anchor: vec![1],
            step: 1,
            bound: 10,
            max_iterations: 1000,
            max_rows: 1_000_000,
        },
        ReferenceRecursiveConfig {
            anchor: vec![2, 5],
            step: 3,
            bound: 15,
            max_iterations: 1000,
            max_rows: 1_000_000,
        },
        ReferenceRecursiveConfig {
            anchor: vec![1, 1], // duplicates
            step: 2,
            bound: 7,
            max_iterations: 1000,
            max_rows: 1_000_000,
        },
        ReferenceRecursiveConfig {
            anchor: vec![100],
            step: 1,
            bound: 50, // early terminate (no iterations generated)
            max_iterations: 1000,
            max_rows: 1_000_000,
        },
    ];

    for (idx, tc) in test_cases.iter().enumerate() {
        let expected = reference_recursive_evaluator(tc).unwrap();

        let mut txn = db.begin().unwrap();
        // Clear oracle_seeds
        txn.execute_statement("DELETE FROM oracle_seeds;").unwrap();
        for (row_id, &val) in tc.anchor.iter().enumerate() {
            txn.execute_statement(&format!(
                "INSERT INTO oracle_seeds (id, v) VALUES ({}, {val});",
                row_id + 1
            ))
            .unwrap();
        }

        let step = tc.step;
        let bound = tc.bound;
        let sql = format!(
            "WITH RECURSIVE r(v) AS (
                SELECT v FROM oracle_seeds
                UNION ALL
                SELECT v + {step} FROM r WHERE v < {bound}
            )
            SELECT v FROM r;"
        );

        let res = txn.execute_sql(&sql).unwrap();
        let actual: Vec<i64> = res
            .rows
            .iter()
            .map(|r| match r.values[0] {
                Value::Int64(v) => v,
                _ => panic!("expected int64"),
            })
            .collect();

        // Compare multiset of values
        let mut expected_sorted = expected.clone();
        expected_sorted.sort();
        let mut actual_sorted = actual.clone();
        actual_sorted.sort();

        assert_eq!(
            actual_sorted, expected_sorted,
            "Mismatch for oracle test case #{idx}"
        );
        txn.commit().unwrap();
    }
}
