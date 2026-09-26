use std::path::PathBuf;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::{Error, Result};
use treedb::query::external_sort::{ExternalSorter, SortConfig};
use treedb::query::operator::{SortDirection, SortKey};
use treedb::query::Expr;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
    temp_dir: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let unique = format!(
            "test_ext_sort_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let base_dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::create_dir_all(&base_dir);
        let db_path = base_dir.join("test.treedb");
        let wal_path = base_dir.join("test.treedb.wal");
        let temp_dir = base_dir.join("temp_runs");
        let _ = std::fs::create_dir_all(&temp_dir);

        Self {
            db_path,
            wal_path,
            temp_dir,
        }
    }

    fn db(&self) -> TreeDb {
        let mut db = if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        };
        db.set_sort_config(SortConfig::new().with_temp_dir(&self.temp_dir));
        db
    }

    fn count_temp_files(&self) -> usize {
        if !self.temp_dir.exists() {
            return 0;
        }
        std::fs::read_dir(&self.temp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "tmp")
                    .unwrap_or(false)
            })
            .count()
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
        if let Some(parent) = self.temp_dir.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn create_sample_schema() -> Schema {
    Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("name", DataType::String, true),
        Column::new("score", DataType::Float64, true),
        Column::new("active", DataType::Boolean, true),
    ])
}

// ============================================================================
// 1. Basic Sorter Tests: Empty, Single, Pre-sorted, Reverse, Duplicates
// ============================================================================

#[test]
fn test_external_sort_basic_cases() -> Result<()> {
    let ctx = TestContext::new("basic");
    let schema = create_sample_schema();
    let sort_keys = vec![SortKey::asc(Expr::Column("id".to_string()))];
    let config = SortConfig::new()
        .with_memory_budget_bytes(100)
        .with_temp_dir(&ctx.temp_dir);
    let sorter = ExternalSorter::new(&schema, &sort_keys, config);

    // 1. Empty input
    let (res_empty, metrics_empty) = sorter.sort(Vec::new())?;
    assert!(res_empty.is_empty());
    assert_eq!(metrics_empty.rows_input, 0);
    assert_eq!(metrics_empty.rows_output, 0);
    assert_eq!(metrics_empty.runs_created, 0);

    // 2. Single row
    let row1 = Row::new(vec![
        Value::Int64(42),
        Value::String("Alice".to_string()),
        Value::Float64(95.5),
        Value::Boolean(true),
    ]);
    let (res_single, metrics_single) = sorter.sort(vec![row1.clone()])?;
    assert_eq!(res_single.len(), 1);
    assert_eq!(res_single[0], row1);
    assert_eq!(metrics_single.rows_input, 1);

    // 3. Pre-sorted input
    let rows_presorted = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("A".to_string()),
            Value::Float64(10.0),
            Value::Boolean(true),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("B".to_string()),
            Value::Float64(20.0),
            Value::Boolean(false),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("C".to_string()),
            Value::Float64(30.0),
            Value::Boolean(true),
        ]),
    ];
    let (res_pre, _) = sorter.sort(rows_presorted.clone())?;
    assert_eq!(res_pre, rows_presorted);

    // 4. Reverse sorted input
    let rows_rev = vec![
        rows_presorted[2].clone(),
        rows_presorted[1].clone(),
        rows_presorted[0].clone(),
    ];
    let (res_rev, _) = sorter.sort(rows_rev)?;
    assert_eq!(res_rev, rows_presorted);

    // 5. Duplicates
    let rows_dup = vec![
        Row::new(vec![
            Value::Int64(5),
            Value::String("D1".to_string()),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("D2".to_string()),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("D3".to_string()),
            Value::Null,
            Value::Null,
        ]),
    ];
    let (res_dup, _) = sorter.sort(rows_dup)?;
    assert_eq!(res_dup.len(), 3);
    assert_eq!(res_dup[0].values[1], Value::String("D1".to_string()));
    assert_eq!(res_dup[1].values[1], Value::String("D2".to_string()));
    assert_eq!(res_dup[2].values[1], Value::String("D3".to_string()));

    Ok(())
}

// ============================================================================
// 2. Ordering Semantics: ASC, DESC, NULLS FIRST/LAST, Float64 NaN, Multi-Key
// ============================================================================

#[test]
fn test_external_sort_ordering_semantics() -> Result<()> {
    let ctx = TestContext::new("ordering");
    let schema = create_sample_schema();
    // Budget set low so spilling activates
    let config = SortConfig::new()
        .with_memory_budget_bytes(80)
        .with_temp_dir(&ctx.temp_dir);

    // Multi-key sort: score DESC, id ASC
    let sort_keys = vec![
        SortKey::desc(Expr::Column("score".to_string())),
        SortKey::asc(Expr::Column("id".to_string())),
    ];
    let sorter = ExternalSorter::new(&schema, &sort_keys, config.clone());

    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("A".to_string()),
            Value::Float64(80.0),
            Value::Boolean(true),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("B".to_string()),
            Value::Float64(95.0),
            Value::Boolean(true),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("C".to_string()),
            Value::Null,
            Value::Boolean(false),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("D".to_string()),
            Value::Float64(95.0),
            Value::Boolean(false),
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("E".to_string()),
            Value::Float64(f64::NAN),
            Value::Boolean(true),
        ]),
    ];

    let (sorted, metrics) = sorter.sort(rows)?;
    assert_eq!(sorted.len(), 5);
    assert!(metrics.runs_created > 0);

    // Default DESC: non-nulls (including NaN) first from highest to lowest, then NULLs last
    // Among score = 95.0: id 2 before id 4
    let ids: Vec<i64> = sorted
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(i) => i,
            _ => panic!("expected int"),
        })
        .collect();

    // In Float64 total_cmp: NaN is greater than all normal numbers.
    // So in DESC: NaN (id 5) -> 95.0 (id 2) -> 95.0 (id 4) -> 80.0 (id 1) -> NULL (id 3)
    assert_eq!(ids, vec![5, 2, 4, 1, 3]);

    // Test explicit NULLS FIRST in DESC
    let sort_keys_nulls_first =
        vec![SortKey::desc(Expr::Column("score".to_string())).with_nulls_first(true)];
    let sorter_nf = ExternalSorter::new(&schema, &sort_keys_nulls_first, config);
    let (sorted_nf, _) = sorter_nf.sort(vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Null,
            Value::Float64(10.0),
            Value::Null,
        ]),
        Row::new(vec![Value::Int64(2), Value::Null, Value::Null, Value::Null]),
        Row::new(vec![
            Value::Int64(3),
            Value::Null,
            Value::Float64(20.0),
            Value::Null,
        ]),
    ])?;
    // NULL should be at index 0
    assert_eq!(sorted_nf[0].values[0], Value::Int64(2));

    Ok(())
}

// ============================================================================
// 3. Spilling Threshold & Execution Metrics
// ============================================================================

#[test]
fn test_external_sort_spilling_threshold_and_metrics() -> Result<()> {
    let ctx = TestContext::new("metrics");
    let schema = create_sample_schema();
    let sort_keys = vec![SortKey::asc(Expr::Column("id".to_string()))];

    // Generate 100 rows
    let mut rows = Vec::new();
    for i in (1..=100).rev() {
        rows.push(Row::new(vec![
            Value::Int64(i),
            Value::String(format!("User_{i:04}")),
            Value::Float64((i as f64) * 1.5),
            Value::Boolean(i % 2 == 0),
        ]));
    }

    // Path A: Large in-memory budget -> No Spilling
    let large_config = SortConfig::new()
        .with_memory_budget_bytes(10 * 1024 * 1024)
        .with_temp_dir(&ctx.temp_dir);
    let sorter_large = ExternalSorter::new(&schema, &sort_keys, large_config);
    let (res_in_mem, metrics_in_mem) = sorter_large.sort(rows.clone())?;
    assert_eq!(metrics_in_mem.rows_input, 100);
    assert_eq!(metrics_in_mem.rows_output, 100);
    assert_eq!(metrics_in_mem.runs_created, 0);
    assert_eq!(metrics_in_mem.bytes_spilled, 0);
    assert_eq!(metrics_in_mem.merge_passes, 0);
    assert!(metrics_in_mem.peak_memory_bytes > 0);

    // Path B: Small in-memory budget (500 bytes) -> Spills to Multiple Runs
    let small_config = SortConfig::new()
        .with_memory_budget_bytes(500)
        .with_temp_dir(&ctx.temp_dir);
    let sorter_small = ExternalSorter::new(&schema, &sort_keys, small_config);
    let (res_spilled, metrics_spilled) = sorter_small.sort(rows)?;
    assert_eq!(metrics_spilled.rows_input, 100);
    assert_eq!(metrics_spilled.rows_output, 100);
    assert!(metrics_spilled.runs_created >= 5);
    assert!(metrics_spilled.bytes_spilled > 0);
    assert!(metrics_spilled.merge_passes >= 1);

    // Exact output row equivalence between in-memory and spilled paths
    assert_eq!(res_in_mem, res_spilled);

    // Verify all temporary files were cleaned up after sort
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

// ============================================================================
// 4. Multi-Pass Merging (runs > max_temp_files)
// ============================================================================

#[test]
fn test_external_sort_multi_pass_merge() -> Result<()> {
    let ctx = TestContext::new("multipass");
    let schema = create_sample_schema();
    let sort_keys = vec![SortKey::asc(Expr::Column("id".to_string()))];

    // Generate 120 rows
    let mut rows = Vec::new();
    for i in (1..=120).rev() {
        rows.push(Row::new(vec![
            Value::Int64(i),
            Value::String(format!("LongStringPayload_{i}")),
            Value::Float64(i as f64),
            Value::Boolean(true),
        ]));
    }

    // Force small batches and small max_temp_files (3 open files max)
    let config = SortConfig::new()
        .with_memory_budget_bytes(250) // creates ~15-20 runs
        .with_max_temp_files(3)
        .with_temp_dir(&ctx.temp_dir);

    let sorter = ExternalSorter::new(&schema, &sort_keys, config);
    let (sorted, metrics) = sorter.sort(rows)?;

    assert_eq!(sorted.len(), 120);
    assert!(metrics.runs_created > 3);
    assert!(
        metrics.merge_passes >= 2,
        "expected multi-pass merge, got passes: {}",
        metrics.merge_passes
    );

    // Verify ordering is strictly 1..=120
    for (idx, r) in sorted.iter().enumerate() {
        assert_eq!(r.values[0], Value::Int64((idx + 1) as i64));
    }

    // Verify cleanup
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

// ============================================================================
// 5. Quota & Resource Limits Error Handling
// ============================================================================

#[test]
fn test_external_sort_quota_exceeded() -> Result<()> {
    let ctx = TestContext::new("quota");
    let schema = create_sample_schema();
    let sort_keys = vec![SortKey::asc(Expr::Column("id".to_string()))];

    let mut rows = Vec::new();
    for i in 1..=50 {
        rows.push(Row::new(vec![
            Value::Int64(i),
            Value::String("x".repeat(200)),
            Value::Null,
            Value::Null,
        ]));
    }

    // Limit disk quota to 1000 bytes, which will be exceeded
    let config = SortConfig::new()
        .with_memory_budget_bytes(100)
        .with_max_temp_bytes(1000)
        .with_temp_dir(&ctx.temp_dir);

    let sorter = ExternalSorter::new(&schema, &sort_keys, config);
    let result = sorter.sort(rows);

    assert!(result.is_err());
    match result.unwrap_err() {
        Error::ExternalSortQuotaExceeded {
            bytes_spilled,
            limit,
        } => {
            assert!(bytes_spilled > limit);
            assert_eq!(limit, 1000);
        }
        other => panic!("expected ExternalSortQuotaExceeded, got {other:?}"),
    }

    // Cleaned up even on error
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

// ============================================================================
// 6. SQL Integration: ORDER BY, LIMIT, OFFSET, JOIN, GROUP BY, WINDOW, CTE, SET
// ============================================================================

fn setup_sql_test_db(db: &mut TreeDb) -> Result<()> {
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE employees (id BIGINT PRIMARY KEY, name TEXT, salary DOUBLE, dept_id BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE departments (dept_id BIGINT PRIMARY KEY, dept_name TEXT);",
    )?;

    txn.execute_statement(
        "INSERT INTO departments (dept_id, dept_name) VALUES (1, 'Engineering'), (2, 'Marketing'), (3, 'Sales');",
    )?;

    for i in 1..=100 {
        let dept = (i % 3) + 1;
        let salary = ((100 - i) as f64) * 1000.0;
        txn.execute_statement(&format!(
            "INSERT INTO employees (id, name, salary, dept_id) VALUES ({i}, 'Emp_{i}', {salary:.2}, {dept});"
        ))?;
    }
    txn.commit()?;
    Ok(())
}

#[test]
fn test_external_sort_sql_order_by_limit_offset() -> Result<()> {
    let ctx = TestContext::new("sql_order_limit");
    let mut db = ctx.db();
    setup_sql_test_db(&mut db)?;

    // Set a tiny sort budget on TreeDb to force external sort spilling on SQL queries
    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(300)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;

    // 1. ORDER BY salary ASC
    let res = txn.execute_sql("SELECT id, salary FROM employees ORDER BY salary ASC;")?;
    assert_eq!(res.rows.len(), 100);
    assert_eq!(res.rows[0].values[0], Value::Int64(100)); // salary 0.0
    assert_eq!(res.rows[99].values[0], Value::Int64(1)); // salary 99000.0

    // 2. ORDER BY salary ASC LIMIT 5
    let res_limit =
        txn.execute_sql("SELECT id, salary FROM employees ORDER BY salary ASC LIMIT 5;")?;
    assert_eq!(res_limit.rows.len(), 5);
    assert_eq!(res_limit.rows[0].values[0], Value::Int64(100));
    assert_eq!(res_limit.rows[4].values[0], Value::Int64(96));

    // 3. ORDER BY salary ASC LIMIT 5 OFFSET 10
    let res_offset =
        txn.execute_sql("SELECT id, salary FROM employees ORDER BY salary ASC LIMIT 5 OFFSET 10;")?;
    assert_eq!(res_offset.rows.len(), 5);
    assert_eq!(res_offset.rows[0].values[0], Value::Int64(90));

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);
    Ok(())
}

#[test]
fn test_external_sort_sql_joins_and_aggregates() -> Result<()> {
    let ctx = TestContext::new("sql_join_agg");
    let mut db = ctx.db();
    setup_sql_test_db(&mut db)?;

    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(400)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;

    // JOIN + ORDER BY
    let res_join = txn.execute_sql(
        "SELECT e.name, d.dept_name, e.salary FROM employees e JOIN departments d ON e.dept_id = d.dept_id ORDER BY d.dept_name ASC, e.salary DESC;",
    )?;
    assert_eq!(res_join.rows.len(), 100);
    assert_eq!(
        res_join.rows[0].values[1],
        Value::String("Engineering".to_string())
    );

    // GROUP BY + HAVING + ORDER BY
    let res_agg = txn.execute_sql(
        "SELECT dept_id, COUNT(id) AS cnt, AVG(salary) AS avg_sal FROM employees GROUP BY dept_id HAVING COUNT(id) > 10 ORDER BY avg_sal DESC;",
    )?;
    assert_eq!(res_agg.rows.len(), 3);

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);
    Ok(())
}

#[test]
fn test_external_sort_sql_windows_ctes_and_set_ops() -> Result<()> {
    let ctx = TestContext::new("sql_adv");
    let mut db = ctx.db();
    setup_sql_test_db(&mut db)?;

    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(350)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;

    // Window + ORDER BY
    let res_win = txn.execute_sql(
        "SELECT id, salary, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY salary DESC) AS rnk FROM employees ORDER BY id ASC LIMIT 10;",
    )?;
    assert_eq!(res_win.rows.len(), 10);
    assert_eq!(res_win.rows[0].values[0], Value::Int64(1));

    // CTE + ORDER BY
    let res_cte = txn.execute_sql(
        "WITH high_earners AS (SELECT id, name, salary FROM employees WHERE salary >= 50000.0) SELECT id, salary FROM high_earners ORDER BY salary ASC;",
    )?;
    assert_eq!(res_cte.rows.len(), 50);
    assert_eq!(res_cte.rows[0].values[1], Value::Float64(50000.0));

    // Set operation + ORDER BY
    let res_set = txn.execute_sql(
        "SELECT id, name FROM employees WHERE id <= 5 UNION SELECT id, name FROM employees WHERE id >= 95 ORDER BY id DESC;",
    )?;
    assert_eq!(res_set.rows.len(), 11);
    assert_eq!(res_set.rows[0].values[0], Value::Int64(100));
    assert_eq!(res_set.rows[10].values[0], Value::Int64(1));

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);
    Ok(())
}

// ============================================================================
// 7. EXPLAIN ANALYZE Diagnostics Integration
// ============================================================================

#[test]
fn test_external_sort_explain_analyze_diagnostics() -> Result<()> {
    let ctx = TestContext::new("explain_analyze");
    let mut db = ctx.db();
    setup_sql_test_db(&mut db)?;

    // Force spilling with small budget
    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(300)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;

    let (res, metrics) =
        txn.execute_sql_with_metrics("SELECT id, salary FROM employees ORDER BY salary ASC;")?;
    assert_eq!(res.rows.len(), 100);

    let root = metrics.root_operator.expect("root operator exists");
    assert_eq!(root.kind, treedb::query::OperatorKind::Project);

    // Check Sort child operator metrics
    let sort_metric = root
        .children
        .iter()
        .find(|c| c.kind == treedb::query::OperatorKind::Sort)
        .expect("sort child metric exists");
    assert_eq!(sort_metric.actual_rows, 100);
    assert!(
        sort_metric.counters.runs_created > 0,
        "runs_created should be > 0"
    );
    assert!(
        sort_metric.counters.bytes_spilled > 0,
        "bytes_spilled should be > 0"
    );
    assert!(
        sort_metric.counters.merge_passes >= 1,
        "merge_passes should be >= 1"
    );

    // Verify EXPLAIN ANALYZE text contains runs_created and bytes_spilled
    let explain_res =
        txn.execute_sql("EXPLAIN ANALYZE SELECT id, salary FROM employees ORDER BY salary ASC;")?;
    let plan_text = match &explain_res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected string"),
    };

    assert!(plan_text.contains("Sort [salary Ascending]"));
    assert!(plan_text.contains("actual_rows: 100"));
    assert!(plan_text.contains("runs_created:"));
    assert!(plan_text.contains("bytes_spilled:"));
    assert!(plan_text.contains("merge_passes:"));

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);
    Ok(())
}

// ============================================================================
// 8. Cleanup Verification on Transaction Abort and Error
// ============================================================================

#[test]
fn test_external_sort_cleanup_on_abort_and_drop() -> Result<()> {
    let ctx = TestContext::new("cleanup");
    let mut db = ctx.db();
    setup_sql_test_db(&mut db)?;

    db.set_sort_config(
        SortConfig::new()
            .with_memory_budget_bytes(200)
            .with_temp_dir(&ctx.temp_dir),
    );

    // 1. Transaction abort
    let mut txn = db.begin_transaction()?;
    let _ = txn.execute_sql("SELECT * FROM employees ORDER BY salary ASC;")?;
    assert_eq!(ctx.count_temp_files(), 0);
    txn.abort()?;
    assert_eq!(ctx.count_temp_files(), 0);

    // 2. Transaction drop
    {
        let mut txn2 = db.begin_transaction()?;
        let _ = txn2.execute_sql("SELECT * FROM employees ORDER BY name DESC;")?;
        assert_eq!(ctx.count_temp_files(), 0);
    }
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

fn assert_rows_equivalent(expected: &[Row], actual: &[Row]) {
    assert_eq!(expected.len(), actual.len(), "row count mismatch");
    for (idx, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            exp.values.len(),
            act.values.len(),
            "column count mismatch at row {idx}"
        );
        for (col_idx, (v1, v2)) in exp.values.iter().zip(act.values.iter()).enumerate() {
            let matches = match (v1, v2) {
                (Value::Float64(f1), Value::Float64(f2)) => {
                    if f1.is_nan() && f2.is_nan() {
                        true
                    } else {
                        f1 == f2
                    }
                }
                (other1, other2) => other1 == other2,
            };
            assert!(
                matches,
                "value mismatch at row {idx}, col {col_idx}: expected {v1:?}, got {v2:?}"
            );
        }
    }
}

// ============================================================================
// 9. Randomized Differential Testing Oracle
// ============================================================================

#[test]
fn test_randomized_external_sort_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("oracle");
    let schema = create_sample_schema();

    // Pseudo-random state generator
    let mut rng_state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next_u64 = || -> u64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    for iter in 0..25 {
        let row_count = (next_u64() % 80 + 20) as usize; // 20 to 100 rows
        let mut rows = Vec::with_capacity(row_count);

        for _ in 0..row_count {
            let id = (next_u64() % 50) as i64 - 25;
            let name_val = match next_u64() % 6 {
                0 => Value::Null,
                _ => Value::String(format!("str_{}", next_u64() % 15)),
            };
            let score_val = match next_u64() % 5 {
                0 => Value::Null,
                1 => Value::Float64(f64::NAN),
                _ => Value::Float64(((next_u64() % 100) as f64) * 0.5 - 25.0),
            };
            let active_val = match next_u64() % 3 {
                0 => Value::Null,
                1 => Value::Boolean(false),
                _ => Value::Boolean(true),
            };

            rows.push(Row::new(vec![
                Value::Int64(id),
                name_val,
                score_val,
                active_val,
            ]));
        }

        // Random sort key combination
        let dir1 = if next_u64() % 2 == 0 {
            SortDirection::Ascending
        } else {
            SortDirection::Descending
        };
        let dir2 = if next_u64() % 2 == 0 {
            SortDirection::Ascending
        } else {
            SortDirection::Descending
        };

        let sort_keys = vec![
            SortKey::new(Expr::Column("name".to_string()), dir1),
            SortKey::new(Expr::Column("id".to_string()), dir2),
        ];

        // Oracle 1: In-Memory (Budget = 100 MB)
        let config_mem = SortConfig::new()
            .with_memory_budget_bytes(100 * 1024 * 1024)
            .with_temp_dir(&ctx.temp_dir);
        let sorter_mem = ExternalSorter::new(&schema, &sort_keys, config_mem);
        let (expected_rows, _) = sorter_mem.sort(rows.clone())?;

        // Oracle 2: Spilled External Sort (Tiny Budget = 60 bytes, small max files = 3)
        let config_spill = SortConfig::new()
            .with_memory_budget_bytes(60)
            .with_max_temp_files(3)
            .with_temp_dir(&ctx.temp_dir);
        let sorter_spill = ExternalSorter::new(&schema, &sort_keys, config_spill);
        let (actual_rows, metrics) = sorter_spill.sort(rows)?;

        assert_rows_equivalent(&expected_rows, &actual_rows);
        assert!(
            metrics.runs_created > 0,
            "Iteration {iter} should have spilled"
        );
        assert_eq!(
            ctx.count_temp_files(),
            0,
            "Iteration {iter} leaked temporary files"
        );
    }

    Ok(())
}

#[test]
fn test_external_sort_large_dataset_and_stability() -> Result<()> {
    let ctx = TestContext::new("large_scale");
    let schema = Schema::new_projected(vec![
        Column::new("category", DataType::String, false),
        Column::new("val", DataType::Int64, false),
        Column::new("extra", DataType::String, true),
    ]);

    let sort_keys = vec![
        SortKey::asc(Expr::Column("category".to_string())),
        SortKey::desc(Expr::Column("val".to_string())),
    ];

    let mut rows = Vec::with_capacity(1000);
    for i in 0..1000 {
        let cat = format!("cat_{:02}", i % 10);
        let val = (i * 7) % 500;
        let extra = format!("payload_data_{i:04}");
        rows.push(Row::new(vec![
            Value::String(cat),
            Value::Int64(val),
            Value::String(extra),
        ]));
    }

    let config = SortConfig::new()
        .with_memory_budget_bytes(2 * 1024) // 2 KB budget forces ~40-50 runs
        .with_max_temp_files(4)
        .with_temp_dir(&ctx.temp_dir);

    let sorter = ExternalSorter::new(&schema, &sort_keys, config);
    let (sorted, metrics) = sorter.sort(rows.clone())?;

    assert_eq!(sorted.len(), 1000);
    assert!(metrics.runs_created > 10);
    assert!(metrics.merge_passes >= 2);
    assert_eq!(ctx.count_temp_files(), 0);

    // Verify ordering
    for i in 1..sorted.len() {
        let ord = treedb::query::operator::compare_rows_by_sort_keys(
            &sorted[i - 1],
            &sorted[i],
            &schema,
            &sort_keys,
        )?;
        assert!(
            ord == std::cmp::Ordering::Less || ord == std::cmp::Ordering::Equal,
            "Ordering violated at index {i}"
        );
    }

    Ok(())
}
