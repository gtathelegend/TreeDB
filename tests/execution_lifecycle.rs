use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::{Error, Result};
use treedb::query::execution::{
    safe_close, ExecutionContext, ExecutionOperator, ExecutionResourceLimits,
    ExecutionTerminationStatus, FilterExecutionOperator, OffsetLimitExecutionOperator,
    OperatorLifecycleState, PhysicalPlanExecutor, PhysicalScanOperator, SortExecutionOperator,
    TopNExecutionOperator,
};
use treedb::query::expr::Expr;
use treedb::query::external_sort::SortConfig;
use treedb::query::metrics::{OperatorKind, OperatorMetrics};
use treedb::query::operator::{SortDirection, SortKey};
use treedb::wal::lsn::TxnId;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_exec_lifecycle_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_exec_lifecycle_{name}.treedb.wal"));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
        Self { db_path, wal_path }
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

fn sample_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("score", DataType::Int64, false),
            Column::new("category", DataType::String, true),
        ],
        0,
    )
    .unwrap()
}

fn sample_rows(count: usize) -> Vec<Row> {
    (0..count)
        .map(|i| {
            Row::new(vec![
                Value::UInt64(i as u64),
                Value::Int64((1000 - i) as i64),
                Value::String(format!("cat_{}", i % 5)),
            ])
        })
        .collect()
}

// ============================================================================
// 1. Canonical Operator Lifecycle Tests
// ============================================================================

#[test]
fn test_operator_lifecycle_valid_transitions() {
    let schema = sample_schema();
    let rows = sample_rows(5);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "test".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(5.0),
        Some(10.0),
    );

    let mut op = PhysicalScanOperator::new(
        "test".to_string(),
        schema.clone(),
        rows.clone(),
        metrics,
        None,
        None,
        false,
    );

    let mut ctx = ExecutionContext::for_txn(TxnId(1));

    // Created
    assert_eq!(op.state(), OperatorLifecycleState::Created);

    // open() -> Opened
    op.open(&mut ctx).unwrap();
    assert_eq!(op.state(), OperatorLifecycleState::Opened);

    // next() -> Running
    let r1 = op.next(&mut ctx).unwrap();
    assert!(r1.is_some());
    assert_eq!(op.state(), OperatorLifecycleState::Running);

    // Drain remaining
    for _ in 1..5 {
        assert!(op.next(&mut ctx).unwrap().is_some());
    }

    // Exhaustion -> Exhausted
    let end = op.next(&mut ctx).unwrap();
    assert!(end.is_none());
    assert_eq!(op.state(), OperatorLifecycleState::Exhausted);

    // close() -> Closed
    op.close(&mut ctx).unwrap();
    assert_eq!(op.state(), OperatorLifecycleState::Closed);

    // Idempotent close()
    assert!(op.close(&mut ctx).is_ok());
    assert_eq!(op.state(), OperatorLifecycleState::Closed);

    // Resource manager must be clean
    assert!(ctx.resource_manager.is_clean());
}

#[test]
fn test_operator_next_before_open_fails() {
    let schema = sample_schema();
    let rows = sample_rows(3);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "test".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(3.0),
        Some(5.0),
    );

    let mut op =
        PhysicalScanOperator::new("test".to_string(), schema, rows, metrics, None, None, false);

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    let err = op.next(&mut ctx).unwrap_err();
    match err {
        Error::InvalidOperatorLifecycleState {
            operator, actual, ..
        } => {
            assert!(operator.contains("Scan"));
            assert_eq!(actual, "Created");
        }
        other => panic!("Expected InvalidOperatorLifecycleState, got {:?}", other),
    }
}

#[test]
fn test_operator_double_open_fails() {
    let schema = sample_schema();
    let rows = sample_rows(3);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "test".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(3.0),
        Some(5.0),
    );

    let mut op =
        PhysicalScanOperator::new("test".to_string(), schema, rows, metrics, None, None, false);

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    op.open(&mut ctx).unwrap();

    let err = op.open(&mut ctx).unwrap_err();
    match err {
        Error::InvalidOperatorLifecycleState { actual, .. } => {
            assert_eq!(actual, "Opened");
        }
        other => panic!("Expected InvalidOperatorLifecycleState, got {:?}", other),
    }

    op.close(&mut ctx).unwrap();
    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 2. Error Propagation and Preservation Tests
// ============================================================================

struct FailingOperator {
    schema: Schema,
    state: OperatorLifecycleState,
    fail_on_step: &'static str,
}

impl FailingOperator {
    fn new(schema: Schema, fail_on_step: &'static str) -> Self {
        Self {
            schema,
            state: OperatorLifecycleState::Created,
            fail_on_step,
        }
    }
}

impl ExecutionOperator for FailingOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, _ctx: &mut ExecutionContext) -> Result<()> {
        if self.fail_on_step == "open" {
            self.state = OperatorLifecycleState::Failed;
            return Err(Error::InternalExecutionError(
                "Injected open failure".to_string(),
            ));
        }
        self.state = OperatorLifecycleState::Opened;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        if self.fail_on_step == "next" {
            self.state = OperatorLifecycleState::Failed;
            return Err(Error::InternalExecutionError(
                "Injected next failure".to_string(),
            ));
        }
        Ok(None)
    }

    fn close(&mut self, _ctx: &mut ExecutionContext) -> Result<()> {
        self.state = OperatorLifecycleState::Closed;
        if self.fail_on_step == "close" {
            return Err(Error::ExecutionCleanup(
                "Injected close cleanup failure".to_string(),
            ));
        }
        Ok(())
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        OperatorMetrics::new(OperatorKind::Filter, None, None)
    }
}

#[test]
fn test_primary_error_preservation_over_cleanup_error() {
    let schema = sample_schema();
    let mut op = FailingOperator::new(schema, "close");
    let mut ctx = ExecutionContext::for_txn(TxnId(1));

    op.open(&mut ctx).unwrap();

    // Primary error occurs
    let primary_error = Error::InternalExecutionError("Primary execution error".to_string());
    let res = safe_close(&mut op, &mut ctx, Some(primary_error));

    // The returned error MUST be the primary execution error, not the secondary cleanup error
    match res {
        Err(Error::InternalExecutionError(msg)) => {
            assert_eq!(msg, "Primary execution error");
        }
        other => panic!("Expected primary error preserved, got {:?}", other),
    }

    // The cleanup error must be captured in context diagnostic list
    assert_eq!(ctx.cleanup_errors.len(), 1);
    assert!(ctx.cleanup_errors[0].contains("Injected close cleanup failure"));
}

#[test]
fn test_child_next_error_propagates_deterministically() {
    let schema = sample_schema();
    let child = Box::new(FailingOperator::new(schema.clone(), "next"));
    let mut filter =
        FilterExecutionOperator::new(child, Expr::Column("id".to_string()), None, None);

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    filter.open(&mut ctx).unwrap();

    let err = filter.next(&mut ctx).unwrap_err();
    match err {
        Error::InternalExecutionError(msg) => {
            assert_eq!(msg, "Injected next failure");
        }
        other => panic!("Expected child error, got {:?}", other),
    }

    filter.close(&mut ctx).unwrap();
    assert_eq!(filter.state(), OperatorLifecycleState::Closed);
    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 3. Backpressure & Row Demand Tests
// ============================================================================

#[test]
fn test_offset_limit_backpressure_stops_pulling_child() {
    let schema = sample_schema();
    let rows = sample_rows(100);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(100.0),
        Some(10.0),
    );

    let scan = Box::new(PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    ));

    // Limit 5, offset 2 -> total consumed from child should be exactly 7!
    let mut limit_op =
        OffsetLimitExecutionOperator::new(scan, Some(2), Some(5), Some(5.0), Some(1.0));

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    limit_op.open(&mut ctx).unwrap();

    let mut emitted = Vec::new();
    while let Some(row) = limit_op.next(&mut ctx).unwrap() {
        emitted.push(row);
    }

    assert_eq!(emitted.len(), 5);
    assert_eq!(emitted[0].values[0], Value::UInt64(2));
    assert_eq!(emitted[4].values[0], Value::UInt64(6));

    let m = limit_op.metrics();
    assert_eq!(m.actual_rows, 5);
    // Crucial check: input_rows must be 7 (offset 2 + limit 5), NOT the full 100 rows!
    assert_eq!(m.input_rows, 7);

    limit_op.close(&mut ctx).unwrap();
    assert_eq!(limit_op.state(), OperatorLifecycleState::Closed);
    assert!(ctx.resource_manager.is_clean());
}

#[test]
fn test_limit_one_immediate_backpressure() {
    let schema = sample_schema();
    let rows = sample_rows(50);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(50.0),
        Some(5.0),
    );

    let scan = Box::new(PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    ));

    let mut limit_op = OffsetLimitExecutionOperator::new(scan, None, Some(1), Some(1.0), Some(1.0));

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    limit_op.open(&mut ctx).unwrap();

    let row = limit_op.next(&mut ctx).unwrap();
    assert!(row.is_some());
    assert_eq!(row.unwrap().values[0], Value::UInt64(0));

    // Next must be None immediately without requesting more rows from child
    assert!(limit_op.next(&mut ctx).unwrap().is_none());

    let m = limit_op.metrics();
    assert_eq!(m.actual_rows, 1);
    assert_eq!(m.input_rows, 1);

    limit_op.close(&mut ctx).unwrap();
    assert!(ctx.resource_manager.is_clean());
}

#[test]
fn test_top_n_bounded_heap_execution() {
    let schema = sample_schema();
    let rows = sample_rows(100);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(100.0),
        Some(10.0),
    );

    let scan = Box::new(PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    ));

    let sort_keys = vec![SortKey {
        expression: Expr::Column("score".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];

    let mut top_n = TopNExecutionOperator::new(
        scan,
        sort_keys,
        Some(5),
        None,
        SortConfig::default(),
        Some(5.0),
        Some(10.0),
    );

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    top_n.open(&mut ctx).unwrap();

    let mut out = Vec::new();
    while let Some(row) = top_n.next(&mut ctx).unwrap() {
        out.push(row);
    }

    assert_eq!(out.len(), 5);
    // Ascending score: scores were 1000 - i, so minimum scores are for i = 99, 98, 97, 96, 95
    assert_eq!(out[0].values[1], Value::Int64(901));
    assert_eq!(out[4].values[1], Value::Int64(905));

    let m = top_n.metrics();
    assert_eq!(m.actual_rows, 5);
    assert_eq!(m.counters.top_n_candidate_capacity, 5);
    assert_eq!(m.counters.top_n_rows_retained, 5);

    top_n.close(&mut ctx).unwrap();
    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 4. Centralized Resource Management and Leak Prevention Tests
// ============================================================================

#[test]
fn test_resource_manager_shared_across_nested_operators() {
    let schema = sample_schema();
    let rows = sample_rows(50);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(50.0),
        Some(5.0),
    );

    let scan = Box::new(PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    ));

    let sort_keys = vec![SortKey {
        expression: Expr::Column("id".to_string()),
        direction: SortDirection::Descending,
        nulls_first: None,
    }];

    let sort_op = Box::new(SortExecutionOperator::new(
        scan,
        sort_keys,
        SortConfig::default(),
        Some(50.0),
        Some(20.0),
    ));

    let limit_op = Box::new(OffsetLimitExecutionOperator::new(
        sort_op,
        Some(5),
        Some(10),
        Some(10.0),
        Some(5.0),
    ));

    let mut ctx = ExecutionContext::for_txn(TxnId(1));

    let (res, op_metrics) =
        PhysicalPlanExecutor::execute_operator_tree(limit_op, &mut ctx).unwrap();
    assert_eq!(res.rows.len(), 10);
    assert_eq!(op_metrics.actual_rows, 10);

    // After complete execution, the resource manager MUST be 100% clean
    assert_eq!(ctx.resource_manager.active_memory_reservations(), 0);
    assert_eq!(ctx.resource_manager.active_temp_files(), 0);
    assert_eq!(ctx.resource_manager.active_temp_bytes(), 0);
    assert!(ctx.resource_manager.is_clean());
    assert_eq!(
        ctx.termination_status,
        ExecutionTerminationStatus::CompletedNormally
    );
}

#[test]
fn test_resource_quota_enforcement() {
    let schema = sample_schema();
    let rows = sample_rows(500);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(500.0),
        Some(50.0),
    );

    let scan = Box::new(PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    ));

    // Set tight limit of 100 bytes
    let limits = ExecutionResourceLimits {
        max_memory_bytes: 100,
        max_temp_bytes: 1024,
        max_temp_files: 5,
    };
    let mut ctx = ExecutionContext::new(1, TxnId(1), limits);

    let res = PhysicalPlanExecutor::execute_operator_tree(scan, &mut ctx);
    match res {
        Err(Error::ResourceLimitExceeded {
            resource, limit, ..
        }) => {
            assert_eq!(resource, "memory");
            assert_eq!(limit, 100);
            assert_eq!(
                ctx.termination_status,
                ExecutionTerminationStatus::FailedResourceLimit
            );
        }
        other => panic!("Expected ResourceLimitExceeded, got {:?}", other),
    }

    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 5. Query Cancellation Tests
// ============================================================================

#[test]
fn test_query_cancellation_during_execution() {
    let schema = sample_schema();
    let rows = sample_rows(100);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(100.0),
        Some(10.0),
    );

    let mut scan = PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    );

    let mut ctx = ExecutionContext::for_txn(TxnId(1));
    scan.open(&mut ctx).unwrap();

    // Pull 5 rows
    for _ in 0..5 {
        assert!(scan.next(&mut ctx).unwrap().is_some());
    }

    // Request cancellation
    ctx.request_cancellation();

    // Next call must return ExecutionCancelled
    let err = scan.next(&mut ctx).unwrap_err();
    match err {
        Error::ExecutionCancelled => {}
        other => panic!("Expected ExecutionCancelled, got {:?}", other),
    }

    // Clean up
    scan.close(&mut ctx).unwrap();
    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 6. State Isolation and Repeated Execution Tests
// ============================================================================

#[test]
fn test_repeated_execution_fresh_operator_state() {
    let schema = sample_schema();
    let rows = sample_rows(20);

    for run_id in 0..5 {
        let metrics = OperatorMetrics::new(
            OperatorKind::TableScan {
                table_name: "items".to_string(),
                access_path: "FullTableScan".to_string(),
            },
            Some(20.0),
            Some(2.0),
        );

        let scan = Box::new(PhysicalScanOperator::new(
            "items".to_string(),
            schema.clone(),
            rows.clone(),
            metrics,
            None,
            None,
            false,
        ));

        let limit_op = Box::new(OffsetLimitExecutionOperator::new(
            scan,
            None,
            Some(3),
            Some(3.0),
            Some(1.0),
        ));

        let mut ctx =
            ExecutionContext::new(run_id, TxnId(run_id), ExecutionResourceLimits::unlimited());
        let (res, m) = PhysicalPlanExecutor::execute_operator_tree(limit_op, &mut ctx).unwrap();

        assert_eq!(res.rows.len(), 3);
        assert_eq!(m.actual_rows, 3);
        assert_eq!(m.input_rows, 3);
        assert!(ctx.resource_manager.is_clean());
    }
}

// ============================================================================
// 7. End-to-End SQL Integration Tests with ExecutionContext
// ============================================================================

#[test]
fn test_sql_execution_with_context_and_early_stop() {
    let ctx_test = TestContext::new("sql_early_stop");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t (id UINT64 PRIMARY KEY, val INT64);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", 100 - i))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut exec_ctx = ExecutionContext::for_txn(TxnId(10));
    let (res, metrics) = db
        .execute_sql_with_context("SELECT id, val FROM t ORDER BY id LIMIT 5;", &mut exec_ctx)
        .unwrap();

    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[4].values[0], Value::UInt64(5));

    // Assert clean resource manager
    assert!(exec_ctx.resource_manager.is_clean());
    assert_eq!(
        exec_ctx.termination_status,
        ExecutionTerminationStatus::CompletedEarly
    );
    assert!(metrics
        .find_first(&|m| m.counters.early_terminated)
        .is_some());
}

#[test]
fn test_sql_execution_cancellation() {
    let ctx_test = TestContext::new("sql_canc");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE nums (x UINT64 PRIMARY KEY);")
        .unwrap();
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO nums VALUES ({i});"))
            .unwrap();
    }
    txn.commit().unwrap();

    let mut exec_ctx = ExecutionContext::for_txn(TxnId(20));
    exec_ctx.request_cancellation();

    let res = db.execute_sql_with_context("SELECT * FROM nums;", &mut exec_ctx);
    match res {
        Err(Error::ExecutionCancelled) => {}
        other => panic!("Expected ExecutionCancelled, got {:?}", other),
    }

    assert!(exec_ctx.resource_manager.is_clean());
}

// ============================================================================
// 8. Adversarial Tests
// ============================================================================

#[test]
fn test_adversarial_large_offset_and_selective_filter() {
    let ctx_test = TestContext::new("adv_offset");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE large (id UINT64 PRIMARY KEY, flag BOOLEAN, num INT64);")
        .unwrap();
    for i in 1..=100 {
        let flag = i % 2 == 0;
        txn.execute_statement(&format!(
            "INSERT INTO large VALUES ({i}, {flag}, {});",
            i * 10
        ))
        .unwrap();
    }
    txn.commit().unwrap();

    let mut exec_ctx = ExecutionContext::for_txn(TxnId(30));
    let (res, _) = db
        .execute_sql_with_context(
            "SELECT id, num FROM large WHERE flag = true ORDER BY id LIMIT 5 OFFSET 40;",
            &mut exec_ctx,
        )
        .unwrap();

    // 50 total flag=true rows (2, 4, 6... 100).
    // Offset 40 skips 40 rows (2 through 80), leaving 82, 84, 86, 88, 90.
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::UInt64(82));
    assert_eq!(res.rows[4].values[0], Value::UInt64(90));
    assert!(exec_ctx.resource_manager.is_clean());
}

// ============================================================================
// 9. Differential Testing
// ============================================================================

#[test]
fn test_differential_equivalence_sql_vs_context() {
    let ctx_test = TestContext::new("diff_eq");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE diff (k UINT64 PRIMARY KEY, v STRING);")
        .unwrap();
    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO diff VALUES ({i}, 'val_{}');", 20 - i))
            .unwrap();
    }
    txn.commit().unwrap();

    let sql = "SELECT k, v FROM diff WHERE k > 5 ORDER BY k LIMIT 7;";

    // Reference execution
    let ref_res = db.execute_sql(sql).unwrap();

    // Context execution
    let mut ctx = ExecutionContext::for_txn(TxnId(40));
    let (ctx_res, _) = db.execute_sql_with_context(sql, &mut ctx).unwrap();

    assert_eq!(ref_res.schema, ctx_res.schema);
    assert_eq!(ref_res.rows.len(), ctx_res.rows.len());
    for (r1, r2) in ref_res.rows.iter().zip(ctx_res.rows.iter()) {
        assert_eq!(r1, r2);
    }
    assert!(ctx.resource_manager.is_clean());
}

// ============================================================================
// 10. Additional Lifecycle & Backpressure Tests
// ============================================================================

#[test]
fn test_operator_double_close_and_close_after_exhaustion() {
    let schema = sample_schema();
    let rows = sample_rows(4);
    let metrics = OperatorMetrics::new(
        OperatorKind::TableScan {
            table_name: "items".to_string(),
            access_path: "FullTableScan".to_string(),
        },
        Some(4.0),
        Some(4.0),
    );

    let mut scan = PhysicalScanOperator::new(
        "items".to_string(),
        schema,
        rows,
        metrics,
        None,
        None,
        false,
    );

    let mut ctx = ExecutionContext::for_txn(TxnId(50));
    scan.open(&mut ctx).unwrap();

    // Drain all rows
    while scan.next(&mut ctx).unwrap().is_some() {}
    assert_eq!(scan.state(), OperatorLifecycleState::Exhausted);

    // First close
    scan.close(&mut ctx).unwrap();
    assert_eq!(scan.state(), OperatorLifecycleState::Closed);

    // Second close (idempotent)
    scan.close(&mut ctx).unwrap();
    assert_eq!(scan.state(), OperatorLifecycleState::Closed);

    assert!(ctx.resource_manager.is_clean());
}

#[test]
fn test_operator_close_after_error_succeeds_and_cleans_resources() {
    let schema = sample_schema();
    let child = Box::new(FailingOperator::new(schema.clone(), "next"));
    let mut filter =
        FilterExecutionOperator::new(child, Expr::Column("id".to_string()), None, None);

    let mut ctx = ExecutionContext::for_txn(TxnId(51));
    filter.open(&mut ctx).unwrap();

    // Trigger error
    assert!(filter.next(&mut ctx).is_err());

    // Close after error must cleanly succeed
    filter.close(&mut ctx).unwrap();
    assert_eq!(filter.state(), OperatorLifecycleState::Closed);
    assert!(ctx.resource_manager.is_clean());
}

#[test]
fn test_semi_join_backpressure_stops_inner_probe() {
    let ctx_test = TestContext::new("semi_join");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE outer_t (id UINT64 PRIMARY KEY, val INT64);")
        .unwrap();
    txn.execute_statement("CREATE TABLE inner_t (id UINT64 PRIMARY KEY, ref_val INT64);")
        .unwrap();
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO outer_t VALUES ({i}, {});", i * 10))
            .unwrap();
    }
    for i in 1..=50 {
        // Many duplicate ref_val matching outer_t
        txn.execute_statement(&format!(
            "INSERT INTO inner_t VALUES ({i}, {});",
            (i % 5 + 1) * 10
        ))
        .unwrap();
    }
    txn.commit().unwrap();

    let mut exec_ctx = ExecutionContext::for_txn(TxnId(52));
    let (res, _) = db
        .execute_sql_with_context(
            "SELECT id, val FROM outer_t WHERE EXISTS (SELECT 1 FROM inner_t WHERE ref_val = outer_t.val);",
            &mut exec_ctx,
        )
        .unwrap();

    assert_eq!(res.rows.len(), 5);
    assert!(exec_ctx.resource_manager.is_clean());
}

// ============================================================================
// 11. Performance & Benchmark Suite Tests
// ============================================================================

#[test]
fn test_execution_benchmark_suite_all_twelve_operators() {
    let ctx_test = TestContext::new("benchmarks");
    let mut db = ctx_test.db();

    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id UINT64 PRIMARY KEY, cat STRING, num INT64);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id UINT64 PRIMARY KEY, fk UINT64, info STRING);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO t1 VALUES ({i}, 'cat_{}', {});",
            i % 5,
            (100 - i) * 10
        ))
        .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, 'info_{i}');"))
            .unwrap();
    }
    txn.commit().unwrap();

    // 12 Operator benchmarks
    let benchmarks = [
        ("1. Full Table Scan", "SELECT * FROM t1;"),
        ("2. Indexed Point Lookup", "SELECT * FROM t1 WHERE id = 42;"),
        (
            "3. Indexed Range Scan",
            "SELECT * FROM t1 WHERE id >= 10 AND id <= 30;",
        ),
        ("4. Filter", "SELECT * FROM t1 WHERE num > 500;"),
        ("5. Sort", "SELECT * FROM t1 ORDER BY num ASC;"),
        ("6. TopN", "SELECT * FROM t1 ORDER BY num ASC LIMIT 10;"),
        (
            "7. Aggregation",
            "SELECT cat, COUNT(*), SUM(num) FROM t1 GROUP BY cat;",
        ),
        (
            "8. Streaming Aggregation",
            "SELECT id, COUNT(*) FROM t1 GROUP BY id;",
        ),
        (
            "9. Hash Join",
            "SELECT t1.id, t2.info FROM t1 JOIN t2 ON t1.id = t2.fk;",
        ),
        (
            "10. Merge Join",
            "SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.id = t2.id;",
        ),
        (
            "11. Set Operation",
            "SELECT id FROM t1 UNION ALL SELECT fk FROM t2;",
        ),
        (
            "12. CTE Materialization",
            "WITH cte AS (SELECT * FROM t1 WHERE id <= 20) SELECT * FROM cte WHERE num > 100;",
        ),
    ];

    for (name, sql) in benchmarks {
        let mut ctx = ExecutionContext::for_txn(TxnId(100));
        let start = std::time::Instant::now();
        let (res, metrics) = db.execute_sql_with_context(sql, &mut ctx).unwrap();
        let elapsed = start.elapsed();

        let rows_count = res.rows.len();
        let duration_secs = elapsed.as_secs_f64().max(1e-6);
        let rows_per_sec = (rows_count as f64) / duration_secs;

        assert!(
            ctx.resource_manager.is_clean(),
            "Resource leak in benchmark: {name}"
        );
        println!(
            "[BENCHMARK] {name: <25} | rows: {: >4} | time: {: >8.2?} | rows/sec: {: >10.0} | peak_mem: {: >6} bytes",
            rows_count,
            elapsed,
            rows_per_sec,
            metrics.counters.peak_memory_bytes
        );
    }
}
