use std::path::PathBuf;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::{Error, Result};
use treedb::query::expr::{AggregateExpr, AggregateFunction, Expr};
use treedb::query::external_aggregate::{
    AggregateConfig, ExternalAggregator, TempAggregateFile, TEMP_AGG_VERSION,
};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    _wal_path: PathBuf,
    temp_dir: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let unique = format!(
            "test_ext_agg_{}_{}_{}",
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
        let temp_dir = base_dir.join("temp_agg_runs");
        let _ = std::fs::create_dir_all(&temp_dir);

        Self {
            db_path,
            _wal_path: wal_path,
            temp_dir,
        }
    }

    fn db(&self) -> TreeDb {
        let mut db = if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        };
        db.set_aggregate_config(AggregateConfig::new().with_temp_dir(&self.temp_dir));
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

fn create_test_schema() -> Schema {
    Schema::new_projected(vec![
        Column::new("id", DataType::Int64, false),
        Column::new("dept", DataType::String, true),
        Column::new("amount", DataType::Int64, true),
        Column::new("score", DataType::Float64, true),
    ])
}

fn assert_rows_equivalent(rows1: &[Row], rows2: &[Row]) {
    assert_eq!(
        rows1.len(),
        rows2.len(),
        "row count mismatch: {} vs {}",
        rows1.len(),
        rows2.len()
    );

    for (i, (r1, r2)) in rows1.iter().zip(rows2.iter()).enumerate() {
        assert_eq!(
            r1.values.len(),
            r2.values.len(),
            "column count mismatch at row {i}"
        );
        for (j, (v1, v2)) in r1.values.iter().zip(r2.values.iter()).enumerate() {
            match (v1, v2) {
                (Value::Float64(f1), Value::Float64(f2)) => {
                    if f1.is_nan() && f2.is_nan() {
                        continue;
                    }
                    assert!(
                        (f1 - f2).abs() < 1e-6,
                        "float mismatch at row {i}, col {j}: {f1} vs {f2}"
                    );
                }
                _ => {
                    assert_eq!(v1, v2, "value mismatch at row {i}, col {j}");
                }
            }
        }
    }
}

fn sort_rows_by_first_col(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| match (&a.values[0], &b.values[0]) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
        (Value::UInt64(x), Value::UInt64(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Float64(x), Value::Float64(y)) => x.total_cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    rows
}

#[test]
fn test_external_aggregate_basic_cases() -> Result<()> {
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Count,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Avg,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Min,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Max,
            expr: Expr::Column("amount".to_string()),
        },
    ];

    // Case 1: Empty input with GROUP BY -> 0 rows
    let config = AggregateConfig::default();
    let agg = ExternalAggregator::new(&schema, &group_by, &aggregates, config.clone());
    let (_, rows, metrics) = agg.aggregate(vec![])?;
    assert_eq!(rows.len(), 0);
    assert_eq!(metrics.rows_input, 0);
    assert_eq!(metrics.rows_output, 0);

    // Case 2: Empty input without GROUP BY (global aggregation) -> 1 row with default states
    let global_agg = ExternalAggregator::new(&schema, &[], &aggregates, config.clone());
    let (_, global_rows, global_metrics) = global_agg.aggregate(vec![])?;
    assert_eq!(global_rows.len(), 1);
    assert_eq!(global_metrics.rows_input, 0);
    assert_eq!(global_metrics.rows_output, 1);
    assert_eq!(global_rows[0].values[0], Value::UInt64(0)); // count(*)
    assert_eq!(global_rows[0].values[1], Value::UInt64(0)); // count(amount)
    assert_eq!(global_rows[0].values[2], Value::Null); // sum(amount)
    assert_eq!(global_rows[0].values[3], Value::Null); // avg(amount)
    assert_eq!(global_rows[0].values[4], Value::Null); // min(amount)
    assert_eq!(global_rows[0].values[5], Value::Null); // max(amount)

    // Case 3: Single row
    let single_row = vec![Row::new(vec![
        Value::Int64(1),
        Value::String("Engineering".to_string()),
        Value::Int64(100),
        Value::Float64(95.5),
    ])];
    let agg_single = ExternalAggregator::new(&schema, &group_by, &aggregates, config);
    let (_, single_out, _) = agg_single.aggregate(single_row)?;
    assert_eq!(single_out.len(), 1);
    assert_eq!(
        single_out[0].values[0],
        Value::String("Engineering".to_string())
    );
    assert_eq!(single_out[0].values[1], Value::UInt64(1)); // count(*)
    assert_eq!(single_out[0].values[2], Value::UInt64(1)); // count(amount)
    assert_eq!(single_out[0].values[3], Value::Int64(100)); // sum
    assert_eq!(single_out[0].values[4], Value::Float64(100.0)); // avg
    assert_eq!(single_out[0].values[5], Value::Int64(100)); // min
    assert_eq!(single_out[0].values[6], Value::Int64(100)); // max

    Ok(())
}

#[test]
fn test_external_aggregate_global_aggregation() -> Result<()> {
    let schema = create_test_schema();
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Count,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Avg,
            expr: Expr::Column("score".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Min,
            expr: Expr::Column("score".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Max,
            expr: Expr::Column("score".to_string()),
        },
    ];

    let input_rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("A".to_string()),
            Value::Int64(10),
            Value::Float64(1.5),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("B".to_string()),
            Value::Null,
            Value::Float64(2.5),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("C".to_string()),
            Value::Int64(30),
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("D".to_string()),
            Value::Int64(20),
            Value::Float64(5.0),
        ]),
    ];

    let config = AggregateConfig::default();
    let agg = ExternalAggregator::new(&schema, &[], &aggregates, config);
    let (_, out_rows, metrics) = agg.aggregate(input_rows)?;
    assert_eq!(out_rows.len(), 1);
    assert_eq!(metrics.groups_created, 1);
    assert_eq!(out_rows[0].values[0], Value::UInt64(4)); // count(*)
    assert_eq!(out_rows[0].values[1], Value::UInt64(3)); // count(amount)
    assert_eq!(out_rows[0].values[2], Value::Int64(60)); // sum(amount) = 10 + 30 + 20
    assert_eq!(out_rows[0].values[3], Value::Float64(3.0)); // avg(score) = (1.5 + 2.5 + 5.0) / 3 = 3.0
    assert_eq!(out_rows[0].values[4], Value::Float64(1.5)); // min(score)
    assert_eq!(out_rows[0].values[5], Value::Float64(5.0)); // max(score)

    Ok(())
}

#[test]
fn test_external_aggregate_grouping_semantics_and_nulls() -> Result<()> {
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
    ];

    let input_rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Null,
            Value::Int64(10),
            Value::Float64(1.0),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("Sales".to_string()),
            Value::Int64(20),
            Value::Float64(2.0),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Null,
            Value::Int64(30),
            Value::Float64(3.0),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("Sales".to_string()),
            Value::Int64(40),
            Value::Float64(4.0),
        ]),
    ];

    let config = AggregateConfig::default();
    let agg = ExternalAggregator::new(&schema, &group_by, &aggregates, config);
    let (_, out_rows, metrics) = agg.aggregate(input_rows)?;
    assert_eq!(out_rows.len(), 2);
    assert_eq!(metrics.groups_created, 2);

    let sorted = sort_rows_by_first_col(out_rows);
    // NULL group
    assert_eq!(sorted[0].values[0], Value::Null);
    assert_eq!(sorted[0].values[1], Value::UInt64(2));
    assert_eq!(sorted[0].values[2], Value::Int64(40));

    // Sales group
    assert_eq!(sorted[1].values[0], Value::String("Sales".to_string()));
    assert_eq!(sorted[1].values[1], Value::UInt64(2));
    assert_eq!(sorted[1].values[2], Value::Int64(60));

    Ok(())
}

#[test]
fn test_external_aggregate_spilling_threshold_and_metrics() -> Result<()> {
    let ctx = TestContext::new("spill_thresh");
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Avg,
            expr: Expr::Column("score".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Min,
            expr: Expr::Column("score".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Max,
            expr: Expr::Column("score".to_string()),
        },
    ];

    let mut input_rows = Vec::new();
    for i in 0..1000 {
        let dept = format!("dept_{:03}", i % 50);
        let amount = (i as i64) * 10;
        let score = (i as f64) * 0.5;
        input_rows.push(Row::new(vec![
            Value::Int64(i as i64),
            Value::String(dept),
            Value::Int64(amount),
            Value::Float64(score),
        ]));
    }

    // 1. In-memory run with large budget (no spilling)
    let large_config = AggregateConfig::default().with_memory_budget_bytes(10 * 1024 * 1024);
    let agg_mem = ExternalAggregator::new(&schema, &group_by, &aggregates, large_config);
    let (_, mem_rows, mem_metrics) = agg_mem.aggregate(input_rows.clone())?;
    assert_eq!(mem_metrics.runs_created, 0);
    assert_eq!(mem_metrics.bytes_spilled, 0);
    assert_eq!(mem_metrics.merge_passes, 0);
    assert_eq!(mem_rows.len(), 50);

    // 2. Tiny budget forcing heavy spilling
    let tiny_config = AggregateConfig::default()
        .with_memory_budget_bytes(256) // tiny 256 byte budget
        .with_temp_dir(&ctx.temp_dir);
    let agg_spill = ExternalAggregator::new(&schema, &group_by, &aggregates, tiny_config);
    let (_, spill_rows, spill_metrics) = agg_spill.aggregate(input_rows)?;

    assert!(spill_metrics.runs_created > 0);
    assert!(spill_metrics.bytes_spilled > 0);
    assert!(spill_metrics.spill_partitions > 0);
    assert_eq!(spill_rows.len(), 50);

    // Verify complete mathematical equivalence between in-memory and spilled aggregation
    let sorted_mem = sort_rows_by_first_col(mem_rows);
    let sorted_spill = sort_rows_by_first_col(spill_rows);
    assert_rows_equivalent(&sorted_mem, &sorted_spill);
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

#[test]
fn test_external_aggregate_multi_pass_partitioning() -> Result<()> {
    let ctx = TestContext::new("multi_pass");
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
    ];

    let mut input_rows = Vec::new();
    for i in 0..500 {
        let dept = format!("unique_dept_{i:04}");
        input_rows.push(Row::new(vec![
            Value::Int64(i as i64),
            Value::String(dept),
            Value::Int64(i as i64),
            Value::Float64(i as f64),
        ]));
    }

    // Reference in-memory run
    let large_config = AggregateConfig::default().with_memory_budget_bytes(10 * 1024 * 1024);
    let agg_mem = ExternalAggregator::new(&schema, &group_by, &aggregates, large_config);
    let (_, mem_rows, _) = agg_mem.aggregate(input_rows.clone())?;

    // Multi-pass run: tiny 128 byte budget and 2 temp files forces recursive sub-partitioning
    let multi_pass_config = AggregateConfig::default()
        .with_memory_budget_bytes(128)
        .with_max_temp_files(2)
        .with_temp_dir(&ctx.temp_dir);
    let agg_multi = ExternalAggregator::new(&schema, &group_by, &aggregates, multi_pass_config);
    let (_, multi_rows, multi_metrics) = agg_multi.aggregate(input_rows)?;

    assert!(multi_metrics.merge_passes >= 1);
    assert_eq!(multi_rows.len(), 500);

    let sorted_mem = sort_rows_by_first_col(mem_rows);
    let sorted_multi = sort_rows_by_first_col(multi_rows);
    assert_rows_equivalent(&sorted_mem, &sorted_multi);
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

#[test]
fn test_external_aggregate_quota_and_pass_limits() {
    let ctx = TestContext::new("limits");
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![AggregateExpr::CountAll];

    let mut input_rows = Vec::new();
    for i in 0..100 {
        input_rows.push(Row::new(vec![
            Value::Int64(i as i64),
            Value::String(format!("dept_{i}")),
            Value::Int64(i as i64),
            Value::Float64(i as f64),
        ]));
    }

    // 1. Quota limit exceeded
    let quota_config = AggregateConfig::default()
        .with_memory_budget_bytes(64)
        .with_max_temp_bytes(10) // only 10 bytes allowed
        .with_temp_dir(&ctx.temp_dir);
    let agg_quota = ExternalAggregator::new(&schema, &group_by, &aggregates, quota_config);
    let err = agg_quota.aggregate(input_rows.clone()).unwrap_err();
    assert!(matches!(err, Error::ExternalAggregateQuotaExceeded { .. }));

    // 2. Max passes limit exceeded
    let pass_config = AggregateConfig::default()
        .with_memory_budget_bytes(32)
        .with_max_temp_files(2)
        .with_max_merge_passes(1)
        .with_temp_dir(&ctx.temp_dir);
    let agg_pass = ExternalAggregator::new(&schema, &group_by, &aggregates, pass_config);
    let err = agg_pass.aggregate(input_rows).unwrap_err();
    assert!(matches!(
        err,
        Error::ExternalAggregateMaxPassesExceeded { .. }
    ));
}

#[test]
fn test_external_aggregate_corruption_detection() {
    let ctx = TestContext::new("corruption");
    let (temp_file, mut file) = TempAggregateFile::create_in(&ctx.temp_dir).unwrap();

    // Write corrupted magic bytes
    use std::io::Write;
    file.write_all(b"BAD!").unwrap();
    file.write_all(&TEMP_AGG_VERSION.to_le_bytes()).unwrap();
    file.write_all(&0u64.to_le_bytes()).unwrap();
    file.flush().unwrap();
    drop(file);

    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![AggregateExpr::CountAll];
    let config = AggregateConfig::default().with_temp_dir(&ctx.temp_dir);
    let agg = ExternalAggregator::new(&schema, &group_by, &aggregates, config);
    let mut metrics = treedb::query::external_aggregate::AggregateExecutionMetrics::default();
    let err = agg
        .process_partition(temp_file, 1, &mut metrics)
        .unwrap_err();
    assert!(matches!(err, Error::ExternalAggregateRunCorrupted(_)));
}

#[test]
fn test_external_aggregate_sql_integration() -> Result<()> {
    let ctx = TestContext::new("sql_integration");
    let mut db = ctx.db();

    // Set tiny aggregation budget to force all SQL aggregations to spill to disk
    db.set_aggregate_config(
        AggregateConfig::default()
            .with_memory_budget_bytes(256)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE emp (id BIGINT PRIMARY KEY, dept TEXT, salary BIGINT, bonus DOUBLE);",
    )?;

    for i in 1..=100 {
        let dept = match i % 3 {
            0 => "Engineering",
            1 => "Sales",
            _ => "Marketing",
        };
        let salary = i * 1000;
        let bonus = (i as f64) * 50.0;
        txn.execute_statement(&format!(
            "INSERT INTO emp (id, dept, salary, bonus) VALUES ({i}, '{dept}', {salary}, {bonus:.2});"
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;

    // 1. GROUP BY + HAVING + ORDER BY
    let query_sql = "SELECT dept, COUNT(*), SUM(salary), AVG(bonus), MIN(salary), MAX(salary) \
                     FROM emp \
                     GROUP BY dept \
                     HAVING SUM(salary) > 50000 \
                     ORDER BY dept ASC;";
    let res = txn.execute_sql(query_sql)?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(
        res.rows[0].values[0],
        Value::String("Engineering".to_string())
    );
    assert_eq!(
        res.rows[1].values[0],
        Value::String("Marketing".to_string())
    );
    assert_eq!(res.rows[2].values[0], Value::String("Sales".to_string()));

    // 2. Aggregation with Joins
    txn.execute_statement(
        "CREATE TABLE dept_info (dept_id BIGINT PRIMARY KEY, dept_name TEXT, budget BIGINT);",
    )?;
    txn.execute_statement("INSERT INTO dept_info (dept_id, dept_name, budget) VALUES (1, 'Engineering', 500000), (2, 'Sales', 300000);")?;

    let join_sql = "SELECT d.dept_name, COUNT(*), SUM(e.salary) \
                    FROM dept_info d JOIN emp e ON d.dept_name = e.dept \
                    GROUP BY d.dept_name \
                    ORDER BY d.dept_name;";
    let join_res = txn.execute_sql(join_sql)?;
    assert_eq!(join_res.rows.len(), 2);
    assert_eq!(
        join_res.rows[0].values[0],
        Value::String("Engineering".to_string())
    );
    assert_eq!(
        join_res.rows[1].values[0],
        Value::String("Sales".to_string())
    );

    // 3. Subquery + Aggregation
    let subq_sql = "SELECT dept, SUM(salary) \
                    FROM emp \
                    WHERE bonus >= (SELECT AVG(bonus) FROM emp) \
                    GROUP BY dept \
                    ORDER BY dept;";
    let subq_res = txn.execute_sql(subq_sql)?;
    assert_eq!(subq_res.rows.len(), 3);

    // 4. CTE + Aggregation
    let cte_sql = "WITH high_earners AS (SELECT * FROM emp WHERE salary >= 60000) \
                   SELECT dept, COUNT(*), AVG(salary) \
                   FROM high_earners \
                   GROUP BY dept \
                   ORDER BY dept;";
    let cte_res = txn.execute_sql(cte_sql)?;
    assert_eq!(cte_res.rows.len(), 3);

    // 5. Window function over Aggregation
    let win_sql = "SELECT dept, SUM(salary), \
                          RANK() OVER (ORDER BY SUM(salary) DESC) \
                   FROM emp \
                   GROUP BY dept \
                   ORDER BY dept;";
    let win_res = txn.execute_sql(win_sql)?;
    assert_eq!(win_res.rows.len(), 3);

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

#[test]
fn test_external_aggregate_explain_analyze_diagnostics() -> Result<()> {
    let ctx = TestContext::new("explain_analyze");
    let mut db = ctx.db();

    db.set_aggregate_config(
        AggregateConfig::default()
            .with_memory_budget_bytes(256)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, grp TEXT, val BIGINT);")?;

    for i in 1..=200 {
        txn.execute_statement(&format!(
            "INSERT INTO t (id, grp, val) VALUES ({i}, 'grp_{}', {i});",
            i % 10
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let explain_sql = "EXPLAIN ANALYZE SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp;";
    let res = txn.execute_sql(explain_sql)?;
    let explain_output = res.rows[0].values[0].to_string();

    assert!(explain_output.contains("Aggregate"));
    assert!(explain_output.contains("actual_rows: 10"));
    assert!(explain_output.contains("group_count: 10"));
    assert!(explain_output.contains("spill_partitions:"));
    assert!(explain_output.contains("runs_created:"));
    assert!(explain_output.contains("bytes_spilled:"));

    txn.commit()?;
    Ok(())
}

#[test]
fn test_external_aggregate_cleanup_on_abort_and_drop() -> Result<()> {
    let ctx = TestContext::new("cleanup");
    let mut db = ctx.db();

    db.set_aggregate_config(
        AggregateConfig::default()
            .with_memory_budget_bytes(128)
            .with_temp_dir(&ctx.temp_dir),
    );

    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, category TEXT);")?;

    for i in 1..=300 {
        txn.execute_statement(&format!(
            "INSERT INTO items (id, category) VALUES ({i}, 'cat_{i}');"
        ))?;
    }

    // Execute spilled aggregation query inside active transaction
    let _ = txn.execute_sql("SELECT category, COUNT(*) FROM items GROUP BY category;")?;

    // Abort transaction
    txn.abort()?;

    // Verify no temporary files remain in the temporary spill directory
    assert_eq!(
        ctx.count_temp_files(),
        0,
        "temporary aggregate spill files leaked after transaction abort"
    );

    Ok(())
}

#[test]
fn test_external_aggregate_prepared_statements_and_plan_cache() -> Result<()> {
    let ctx = TestContext::new("prepared_agg");
    let mut db = ctx.db();

    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE sales (id BIGINT PRIMARY KEY, region TEXT, amount BIGINT);",
    )?;

    for i in 1..=200 {
        txn.execute_statement(&format!(
            "INSERT INTO sales (id, region, amount) VALUES ({i}, 'region_{}', {i});",
            i % 5
        ))?;
    }
    txn.commit()?;

    let mut txn = db.begin_transaction()?;
    let prep = txn.prepare_sql(
        "SELECT region, SUM(amount) FROM sales WHERE amount > ? GROUP BY region ORDER BY region;",
    )?;

    // First execution: with parameter amount > 50 using default large budget
    let res1 = txn.execute_prepared_query(&prep, &[Value::Int64(50)])?;
    assert_eq!(res1.rows.len(), 5);

    // Switch to tiny budget forcing external spill
    txn.set_aggregate_config(
        AggregateConfig::default()
            .with_memory_budget_bytes(128)
            .with_temp_dir(&ctx.temp_dir),
    );

    // Second execution reusing the exact same cached plan, but executing spilled
    let res2 = txn.execute_prepared_query(&prep, &[Value::Int64(50)])?;
    assert_eq!(res2.rows.len(), 5);

    assert_rows_equivalent(&res1.rows, &res2.rows);

    txn.commit()?;
    assert_eq!(ctx.count_temp_files(), 0);

    Ok(())
}

#[test]
fn test_randomized_external_aggregate_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("differential_oracle");
    let schema = create_test_schema();
    let group_by = vec![Expr::Column("dept".to_string())];
    let aggregates = vec![
        AggregateExpr::CountAll,
        AggregateExpr::Function {
            func: AggregateFunction::Count,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Sum,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Avg,
            expr: Expr::Column("score".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Min,
            expr: Expr::Column("amount".to_string()),
        },
        AggregateExpr::Function {
            func: AggregateFunction::Max,
            expr: Expr::Column("score".to_string()),
        },
    ];

    let mut rng_state: u64 = 0x1234_5678_9ABC_DEF0;
    let mut next_u64 = || -> u64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    for iter in 0..10 {
        let num_rows = (next_u64() % 200 + 50) as usize;
        let num_depts = (next_u64() % 20 + 3) as usize;

        let mut rows = Vec::with_capacity(num_rows);
        for i in 0..num_rows {
            let dept = if next_u64() % 10 == 0 {
                Value::Null
            } else {
                Value::String(format!("dept_{}", next_u64() as usize % num_depts))
            };

            let amount = if next_u64() % 8 == 0 {
                Value::Null
            } else {
                Value::Int64((next_u64() % 10000) as i64 - 5000)
            };

            let score = if next_u64() % 8 == 0 {
                Value::Null
            } else {
                Value::Float64(((next_u64() % 1000) as f64) * 0.5 - 250.0)
            };

            rows.push(Row::new(vec![Value::Int64(i as i64), dept, amount, score]));
        }

        // 1. In-memory execution (large budget)
        let large_config = AggregateConfig::default().with_memory_budget_bytes(10 * 1024 * 1024);
        let agg_mem = ExternalAggregator::new(&schema, &group_by, &aggregates, large_config);
        let (_, mem_rows, _) = agg_mem.aggregate(rows.clone())?;

        // 2. External spill execution (tiny budget)
        let tiny_config = AggregateConfig::default()
            .with_memory_budget_bytes(128)
            .with_max_temp_files(4)
            .with_temp_dir(&ctx.temp_dir);
        let agg_spill = ExternalAggregator::new(&schema, &group_by, &aggregates, tiny_config);
        let (_, spill_rows, spill_metrics) = agg_spill.aggregate(rows)?;

        assert!(
            spill_metrics.runs_created > 0,
            "iteration {iter} should have spilled"
        );

        let sorted_mem = sort_rows_by_first_col(mem_rows);
        let sorted_spill = sort_rows_by_first_col(spill_rows);
        assert_rows_equivalent(&sorted_mem, &sorted_spill);
        assert_eq!(ctx.count_temp_files(), 0);
    }

    Ok(())
}
