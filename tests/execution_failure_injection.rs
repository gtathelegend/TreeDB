use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::query::execution::{
    ExecutionContext, ExecutionOperator, ExecutionResourceLimits, OperatorLifecycleState,
    PhysicalScanOperator,
};
use treedb::query::metrics::{OperatorKind, OperatorMetrics};
use treedb::query::reference::assert_differential_equivalence;
use treedb::query::{AggregateConfig, JoinConfig, SortConfig};
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_fail_inject_{name}.treedb"));
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

// -----------------------------------------------------------------------------
// 1. Resource Budget Differential Testing (Section 32)
// -----------------------------------------------------------------------------

#[test]
fn test_resource_budget_differential_equivalence() {
    let ctx = TestContext::new("budget_differential");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE items (
            id UINT64 PRIMARY KEY,
            cat STRING,
            price INT64,
            rating FLOAT64
        )",
    )
    .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=300 {
        let cat = match i % 4 {
            0 => "'A'",
            1 => "'B'",
            2 => "'C'",
            _ => "'D'",
        };
        let price = (i * 37) % 500;
        let rating = ((i % 50) as f64) / 10.0;
        let sql = format!("INSERT INTO items VALUES ({i}, {cat}, {price}, {rating})");
        txn.execute_statement(&sql).unwrap();
    }
    txn.commit().unwrap();

    let queries = [
        "SELECT cat, COUNT(*), SUM(price), AVG(rating) FROM items GROUP BY cat ORDER BY cat",
        "SELECT id, cat, price FROM items ORDER BY price DESC, id ASC LIMIT 25",
        "SELECT id, price, rating FROM items WHERE price > 200 ORDER BY price ASC",
    ];

    // Budgets: Large (64MB), Moderate (1MB), and Tiny (256 bytes forcing spilling)
    let budgets = [64 * 1024 * 1024, 1024 * 1024, 256];

    for &budget in &budgets {
        db.set_sort_config(SortConfig::new().with_memory_budget_bytes(budget));
        db.set_aggregate_config(AggregateConfig::new().with_memory_budget_bytes(budget));
        db.set_join_config(JoinConfig::new().with_memory_budget_bytes(budget));

        let mut txn = db.begin_transaction().unwrap();
        for sql in queries {
            let opt = txn.execute_sql(sql).unwrap();
            let ref_res = txn.execute_sql_reference(sql).unwrap();
            let require_order = sql.contains("ORDER BY");
            assert_differential_equivalence(&opt, &ref_res, require_order);
        }
        txn.commit().unwrap();
    }
}

// -----------------------------------------------------------------------------
// 2. Forced Spill Operators vs In-Memory Differential Testing (Section 33)
// -----------------------------------------------------------------------------

#[test]
fn test_forced_spill_operators_differential() {
    let ctx = TestContext::new("forced_spill");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE t_left (id UINT64 PRIMARY KEY, k INT64, val STRING)")
        .unwrap();
    db.execute_statement("CREATE TABLE t_right (id UINT64 PRIMARY KEY, k INT64, descr STRING)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=150 {
        let k = i % 15;
        txn.execute_statement(&format!("INSERT INTO t_left VALUES ({i}, {k}, 'L_{i}')"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t_right VALUES ({i}, {k}, 'R_{i}')"))
            .unwrap();
    }
    txn.commit().unwrap();

    let test_query_sort = "SELECT id, k, val FROM t_left ORDER BY k ASC, id DESC";
    let test_query_agg = "SELECT k, COUNT(*), MIN(val), MAX(val) FROM t_left GROUP BY k ORDER BY k";
    let test_query_join =
        "SELECT l.id, l.k, r.descr FROM t_left l INNER JOIN t_right r ON l.k = r.k";

    // 1. In-memory execution
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(64 * 1024 * 1024));
    db.set_aggregate_config(AggregateConfig::new().with_memory_budget_bytes(64 * 1024 * 1024));
    db.set_join_config(JoinConfig::new().with_memory_budget_bytes(64 * 1024 * 1024));

    let mut txn_mem = db.begin_transaction().unwrap();
    let mem_sort = txn_mem.execute_sql(test_query_sort).unwrap();
    let mem_agg = txn_mem.execute_sql(test_query_agg).unwrap();
    let mem_join = txn_mem.execute_sql(test_query_join).unwrap();

    // Reference execution
    let ref_sort = txn_mem.execute_sql_reference(test_query_sort).unwrap();
    let ref_agg = txn_mem.execute_sql_reference(test_query_agg).unwrap();
    let ref_join = txn_mem.execute_sql_reference(test_query_join).unwrap();

    assert_differential_equivalence(&mem_sort, &ref_sort, true);
    assert_differential_equivalence(&mem_agg, &ref_agg, true);
    assert_differential_equivalence(&mem_join, &ref_join, false);
    txn_mem.commit().unwrap();

    // 2. Forced external spill execution (tiny budget 64 bytes)
    db.set_sort_config(SortConfig::new().with_memory_budget_bytes(64));
    db.set_aggregate_config(AggregateConfig::new().with_memory_budget_bytes(64));
    db.set_join_config(JoinConfig::new().with_memory_budget_bytes(64));

    let mut txn_spill = db.begin_transaction().unwrap();
    let spill_sort = txn_spill.execute_sql(test_query_sort).unwrap();
    let spill_agg = txn_spill.execute_sql(test_query_agg).unwrap();
    let spill_join = txn_spill.execute_sql(test_query_join).unwrap();

    // Spilled output MUST be differentially identical to reference
    assert_differential_equivalence(&spill_sort, &ref_sort, true);
    assert_differential_equivalence(&spill_agg, &ref_agg, true);
    assert_differential_equivalence(&spill_join, &ref_join, false);
    txn_spill.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 3. Deterministic Failure Injection & Clean Recovery (Section 34)
// -----------------------------------------------------------------------------

#[test]
fn test_deterministic_failure_injection_recovery() {
    let ctx = TestContext::new("failure_injection");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE accounts (id UINT64 PRIMARY KEY, bal INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (1, 100)")
        .unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (2, 200)")
        .unwrap();
    txn.commit().unwrap();

    // 1. Strict memory limit failure injection
    let limits = ExecutionResourceLimits {
        max_memory_bytes: 1,
        max_temp_bytes: usize::MAX,
        max_temp_files: usize::MAX,
    };
    let mut exec_ctx = ExecutionContext::new(1, TxnId(1), limits);

    // Reserve 10 bytes -> triggers memory limit exceeded
    let res = exec_ctx.resource_manager.reserve_memory(10);
    assert!(
        res.is_err(),
        "Exceeding memory limit must fail deterministically"
    );

    // 2. Verify clean state after failure
    assert!(
        exec_ctx.resource_manager.is_clean(),
        "Resource manager must be clean after failed reservation"
    );

    // 3. Database and transaction remain fully functional
    let mut txn2 = db.begin_transaction().unwrap();
    txn2.execute_statement("INSERT INTO accounts VALUES (3, 300)")
        .unwrap();
    let q = txn2.execute_sql("SELECT COUNT(*) FROM accounts").unwrap();
    assert_eq!(q.rows[0].values[0], Value::UInt64(3));
    txn2.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 4. Cooperative Cancellation & Deterministic Cleanup (Section 35)
// -----------------------------------------------------------------------------

#[test]
fn test_cooperative_cancellation_resource_cleanup() {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    let rows: Vec<Row> = (1..=100)
        .map(|i| Row::new(vec![Value::UInt64(i), Value::Int64(i as i64 * 10)]))
        .collect();

    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "test".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(100.0),
        Some(100.0),
    );

    let mut op =
        PhysicalScanOperator::new("test".to_string(), schema, rows, metrics, None, None, false);

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    op.open(&mut ctx).unwrap();

    // Read 5 rows
    for _ in 0..5 {
        assert!(op.next(&mut ctx).unwrap().is_some());
    }

    // Trigger cooperative cancellation
    ctx.request_cancellation();
    assert!(ctx.cancellation_requested);

    // Subsequent next() must return ExecutionCancelled error
    let next_res = op.next(&mut ctx);
    assert!(matches!(next_res, Err(Error::ExecutionCancelled)));

    // Cleanup via close()
    op.close(&mut ctx).unwrap();
    assert_eq!(op.state(), OperatorLifecycleState::Closed);
    assert!(ctx.resource_manager.is_clean());
}

// -----------------------------------------------------------------------------
// 5. Plan Cache Invalidation & Parameter Stress (Section 36)
// -----------------------------------------------------------------------------

#[test]
fn test_plan_cache_invalidation_and_correctness_stress() {
    let ctx = TestContext::new("plan_cache_stress");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE users (id UINT64 PRIMARY KEY, age INT64, name STRING)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, {i}, 'U_{i}')"))
            .unwrap();
    }
    txn.commit().unwrap();

    let sql = "SELECT id, name FROM users WHERE age > ? ORDER BY id LIMIT 5";
    let stmt = db.prepare_sql(sql).unwrap();

    // 1. Repeated execution with varying parameters
    for age in [10, 20, 30, 40] {
        let opt = db
            .execute_prepared_query(&stmt, &[Value::Int64(age)])
            .unwrap();

        let mut txn = db.begin_transaction().unwrap();
        let ref_sql = format!("SELECT id, name FROM users WHERE age > {age} ORDER BY id LIMIT 5");
        let ref_res = txn.execute_sql_reference(&ref_sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, true);
        txn.commit().unwrap();
    }

    // 2. DDL mutation invalidates catalog generation & plan cache
    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("CREATE INDEX idx_users_age ON users (age)")
        .unwrap();
    txn.commit().unwrap();

    // 3. Execution after index creation re-prepares and remains correct
    let stmt_after = db.prepare_sql(sql).unwrap();
    let opt_after_idx = db
        .execute_prepared_query(&stmt_after, &[Value::Int64(25)])
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let ref_after_idx = txn
        .execute_sql_reference("SELECT id, name FROM users WHERE age > 25 ORDER BY id LIMIT 5")
        .unwrap();
    assert_differential_equivalence(&opt_after_idx, &ref_after_idx, true);
    txn.commit().unwrap();
}

// -----------------------------------------------------------------------------
// 6. Adaptive Replanning & Feedback Correctness (Section 37)
// -----------------------------------------------------------------------------

#[test]
fn test_adaptive_replanning_feedback_correctness() {
    let ctx = TestContext::new("adaptive_replanning");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE data_stream (id UINT64 PRIMARY KEY, val INT64)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=200 {
        txn.execute_statement(&format!("INSERT INTO data_stream VALUES ({i}, {i})"))
            .unwrap();
    }
    txn.commit().unwrap();

    let sql = "SELECT id, val FROM data_stream WHERE val <= 180 ORDER BY id LIMIT 10";

    // Execute multiple iterations where feedback updates selectivity
    for _ in 0..5 {
        let mut txn = db.begin_transaction().unwrap();
        let opt = txn.execute_sql(sql).unwrap();
        let ref_res = txn.execute_sql_reference(sql).unwrap();
        assert_differential_equivalence(&opt, &ref_res, true);
        txn.commit().unwrap();
    }
}

// -----------------------------------------------------------------------------
// 7. Physical Fingerprint Determinism (Section 38)
// -----------------------------------------------------------------------------

#[test]
fn test_physical_fingerprint_determinism() {
    let ctx = TestContext::new("fingerprint_determinism");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE fp_test (id UINT64 PRIMARY KEY, a INT64, b STRING)")
        .unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let sql = "SELECT id, a FROM fp_test WHERE a > 10 ORDER BY id";

    let plan1 = txn.plan_sql(sql).unwrap();
    let plan2 = txn.plan_sql(sql).unwrap();

    assert_eq!(
        plan1.fingerprint(),
        plan2.fingerprint(),
        "Identical physical plans must have deterministic fingerprints"
    );
}

// -----------------------------------------------------------------------------
// 8. Workload Performance Benchmarks (Section 46)
// -----------------------------------------------------------------------------

#[test]
fn test_workload_performance_benchmarks() {
    let ctx = TestContext::new("benchmarks");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE bench_records (
            id UINT64 PRIMARY KEY,
            k INT64,
            v STRING
        )",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_bench_k ON bench_records (k)")
        .unwrap();

    let n = 200;
    let mut txn = db.begin_transaction().unwrap();
    for i in 1..=n {
        let k = i % 50;
        let sql = format!("INSERT INTO bench_records VALUES ({i}, {k}, 'val_{i}')");
        txn.execute_statement(&sql).unwrap();
    }
    txn.commit().unwrap();

    let benchmarks = [
        ("Full Scan", "SELECT * FROM bench_records"),
        ("Filter", "SELECT * FROM bench_records WHERE k > 25"),
        ("Index Scan", "SELECT * FROM bench_records WHERE k = 10"),
        (
            "Top-N",
            "SELECT * FROM bench_records ORDER BY k ASC LIMIT 10",
        ),
        (
            "Sort",
            "SELECT * FROM bench_records ORDER BY k ASC, id DESC",
        ),
        (
            "Aggregation",
            "SELECT k, COUNT(*), MIN(id), MAX(id) FROM bench_records GROUP BY k",
        ),
    ];

    println!("\n========== TreeDB Phase 66 Performance Benchmarks (N={n}) ==========");
    for (name, sql) in benchmarks {
        let mut latencies = Vec::with_capacity(5);
        let mut rows_count = 0;

        for _ in 0..5 {
            let mut txn = db.begin_transaction().unwrap();
            let start = Instant::now();
            let res = txn.execute_sql(sql).unwrap();
            latencies.push(start.elapsed());
            rows_count = res.rows.len();
        }

        latencies.sort();
        let median = latencies[latencies.len() / 2];
        let duration_secs = median.as_secs_f64();
        let throughput = if duration_secs > 0.0 {
            (rows_count as f64) / duration_secs
        } else {
            0.0
        };

        println!(
            "{:<15} | Median: {:>8.3?} | Rows: {:>6} | Throughput: {:>10.0} rows/s",
            name, median, rows_count, throughput
        );
    }
    println!("====================================================================\n");
}
