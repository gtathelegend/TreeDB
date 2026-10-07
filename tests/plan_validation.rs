use tempfile::TempDir;
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Result;
use treedb::query::expr::{AggregateExpr, AggregateFunction, BinaryOp, Expr};
use treedb::query::external_multi_index::MultiIndexExecutionMode;
use treedb::query::operator::SortDirection;
use treedb::query::optimizer::{
    AccessPath, JoinStrategy, MultiIndexChildScan, MultiIndexStrategy, SecondaryScanType,
};
use treedb::query::physical_properties::{OrderingColumn, OrderingProperty, RowGoal};
use treedb::query::plan_cache::{PlanValidity, ResourcePlanningSignature};
use treedb::query::validation::{PhysicalPlanValidator, PlanValidationError};
use treedb::query::SortKey;
use treedb::sql::ast::JoinType;
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

fn setup_test_schema(db: &mut TreeDb) -> Result<()> {
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, email VARCHAR, age BIGINT, score DOUBLE);",
    )?;
    tx.execute_statement("CREATE INDEX idx_users_email ON users (email);")?;
    tx.execute_statement("CREATE INDEX idx_users_age_score ON users (age, score);")?;

    tx.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, amount DOUBLE, status VARCHAR);",
    )?;
    tx.execute_statement("CREATE INDEX idx_orders_user ON orders (user_id);")?;

    tx.execute_statement("INSERT INTO users VALUES (1, 'Alice', 'alice@test.com', 25, 95.5);")?;
    tx.execute_statement("INSERT INTO users VALUES (2, 'Bob', 'bob@test.com', 30, 80.0);")?;
    tx.execute_statement("INSERT INTO users VALUES (3, 'Charlie', 'charlie@test.com', 35, 88.0);")?;

    tx.execute_statement("INSERT INTO orders VALUES (101, 1, 150.0, 'shipped');")?;
    tx.execute_statement("INSERT INTO orders VALUES (102, 1, 200.0, 'completed');")?;
    tx.execute_statement("INSERT INTO orders VALUES (103, 2, 50.0, 'pending');")?;

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 1. Basic Access Path Validation
// ============================================================================

#[test]
fn test_valid_access_paths() -> Result<()> {
    let ctx = TestContext::new("valid_access_paths");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;

    // 1. Full Table Scan
    let plan = tx.plan_sql("SELECT * FROM users;")?;
    let validated = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(validated.is_ok(), "Full table scan must pass validation");

    // 2. Primary Key Scan
    let plan = tx.plan_sql("SELECT * FROM users WHERE id = 1;")?;
    let validated = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(validated.is_ok(), "Primary key scan must pass validation");

    // 3. Secondary Index Scan
    let plan = tx.plan_sql("SELECT * FROM users WHERE email = 'alice@test.com';")?;
    let validated = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(
        validated.is_ok(),
        "Secondary index scan must pass validation"
    );

    // 4. Composite Index Scan
    let plan = tx.plan_sql("SELECT * FROM users WHERE age = 25 AND score > 90.0;")?;
    let validated = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(
        validated.is_ok(),
        "Composite index scan must pass validation"
    );

    // 5. Covering Scan / Index Only Scan
    let plan = tx.plan_sql("SELECT age, score FROM users WHERE age > 20;")?;
    let validated = PhysicalPlanValidator::validate(&plan, &val_ctx);
    assert!(validated.is_ok(), "Index scan must pass validation");

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 2. Column Demand & Scope Violations
// ============================================================================

#[test]
fn test_column_demand_violations() -> Result<()> {
    let ctx = TestContext::new("col_demand_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;
    let plan = tx.plan_sql("SELECT id, name FROM users;")?;

    // 1. Required column not in table schema
    let mut mutated = plan.clone();
    mutated
        .base_table
        .required_columns
        .push("non_existent_column".to_string());
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(
        err,
        PlanValidationError::RequiredColumnsViolation { .. }
    ));

    // 2. Projection references unavailable column
    let mut mutated2 = plan.clone();
    mutated2
        .projections
        .push((Expr::Column("ghost_col".to_string()), None));
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(err2, PlanValidationError::MissingColumn { .. }));

    // 3. Covering payload marked for column absent from index
    let mut mutated3 = plan.clone();
    mutated3.base_table.access_path = AccessPath::IndexOnlyScan {
        index_name: "idx_users_email".to_string(),
        index_id: treedb::IndexId(1),
        scan_type: SecondaryScanType::PointLookup(Value::String("alice@test.com".to_string())),
        covered_columns: vec!["email".to_string()],
    };
    mutated3.base_table.covered_columns = vec!["score".to_string()]; // score is NOT in idx_users_email
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx).unwrap_err();
    assert!(matches!(
        err3,
        PlanValidationError::InvalidCoveringPayload { .. }
    ));

    // 4. Index-only scan requiring column not in covered payload
    let mut mutated4 = plan.clone();
    mutated4.base_table.access_path = AccessPath::IndexOnlyScan {
        index_name: "idx_users_email".to_string(),
        index_id: treedb::IndexId(1),
        scan_type: SecondaryScanType::PointLookup(Value::String("alice@test.com".to_string())),
        covered_columns: vec!["email".to_string()],
    };
    mutated4.base_table.required_columns = vec!["name".to_string()]; // name is NOT in idx_users_email
    let err4 = PhysicalPlanValidator::validate(&mutated4, &val_ctx).unwrap_err();
    assert!(matches!(
        err4,
        PlanValidationError::RequiredColumnsViolation { .. }
    ));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 3. Ordering Property Violations
// ============================================================================

#[test]
fn test_ordering_violations() -> Result<()> {
    let ctx = TestContext::new("ordering_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;
    let plan = tx.plan_sql("SELECT * FROM users ORDER BY id ASC;")?;

    // 1. Wrong direction: declares Descending when stream provides Ascending without sort
    let mut mutated = plan.clone();
    mutated.order_by_sort_required = false;
    mutated.order_by = vec![SortKey::desc(Expr::Column("id".to_string()))];
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(err, PlanValidationError::OrderingViolation { .. }));

    // 2. Unsafe MultiIndex scan claiming ordering
    let mut mutated2 = plan.clone();
    mutated2.base_table.access_path = AccessPath::MultiIndexScan {
        strategy: MultiIndexStrategy::Intersection,
        child_scans: vec![MultiIndexChildScan {
            index_name: "idx_users_email".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: SecondaryScanType::PointLookup(Value::String("a@b.c".to_string())),
            estimated_rows: 1.0,
            cost: 1.0,
        }],
        execution_mode: MultiIndexExecutionMode::InMemory,
    };
    mutated2.order_by_sort_required = false;
    mutated2.order_by = vec![SortKey::asc(Expr::Column("id".to_string()))];
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(
        err2,
        PlanValidationError::OrderingViolation { .. }
    ));

    // 3. Streaming aggregate receiving unordered input
    let mut agg_plan = tx.plan_sql("SELECT age, COUNT(*) FROM users GROUP BY age;")?;
    agg_plan.aggregate_strategy = Some(treedb::query::optimizer::AggregateStrategy::Streaming {
        sort_required: false,
    });
    agg_plan.base_table.access_path = AccessPath::FullTableScan; // full scan on users gives id ordering, not age
    let err3 = PhysicalPlanValidator::validate(&agg_plan, &val_ctx).unwrap_err();
    assert!(matches!(
        err3,
        PlanValidationError::InvalidAggregate { .. }
            | PlanValidationError::OrderingViolation { .. }
    ));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 4. Row-Goal Contract Violations
// ============================================================================

#[test]
fn test_row_goal_violations() -> Result<()> {
    let ctx = TestContext::new("row_goal_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;
    let plan = tx.plan_sql("SELECT * FROM users LIMIT 5 OFFSET 10;")?;

    // 1. Candidate rows < required output rows
    let mut mutated = plan.clone();
    mutated.row_goal = Some(RowGoal::new(5, 10)); // Impossible: candidate must be >= required
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(err, PlanValidationError::RowGoalViolation { .. }));

    // 2. Candidate rows != LIMIT + OFFSET
    let mut mutated2 = plan.clone();
    mutated2.row_goal = Some(RowGoal::new(20, 5)); // 5 + 10 = 15, not 20
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(err2, PlanValidationError::RowGoalViolation { .. }));

    // 3. Early termination claimed without row goal
    let mut mutated3 = plan.clone();
    mutated3.base_table.early_termination_possible = true;
    mutated3.base_table.row_goal = None;
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx).unwrap_err();
    assert!(matches!(err3, PlanValidationError::RowGoalViolation { .. }));

    // 4. Early termination claimed on Left Outer Join
    let join_plan = tx
        .plan_sql("SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;")?;
    let mut mutated4 = join_plan.clone();
    mutated4.joins[0].early_termination_possible = true;
    mutated4.joins[0].row_goal = Some(RowGoal::new(5, 5));
    let err4 = PhysicalPlanValidator::validate(&mutated4, &val_ctx).unwrap_err();
    assert!(matches!(err4, PlanValidationError::RowGoalViolation { .. }));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 5. Join Violations
// ============================================================================

#[test]
fn test_join_violations() -> Result<()> {
    let ctx = TestContext::new("join_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;
    let join_plan = tx.plan_sql(
        "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id;",
    )?;

    // 1. Join ON condition references missing column
    let mut mutated = join_plan.clone();
    mutated.joins[0].on = Expr::Binary {
        left: Box::new(Expr::Column("u.id".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("o.ghost_col".to_string())),
    };
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(err, PlanValidationError::MissingColumn { .. }));

    // 2. IndexProbe references index belonging to another table
    let mut mutated2 = join_plan.clone();
    mutated2.joins[0].strategy = JoinStrategy::IndexProbe;
    mutated2.joins[0].index_name = Some("idx_users_email".to_string()); // belongs to users, but right table is orders!
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(
        err2,
        PlanValidationError::InvalidIndexReference { .. }
    ));

    // 3. Runtime filter planned on Left Join
    let mut mutated3 = join_plan.clone();
    mutated3.joins[0].join_type = JoinType::Left;
    mutated3.joins[0].runtime_filter = Some(treedb::query::runtime_filter::RuntimeFilterPlan {
        filter_type: treedb::query::runtime_filter::RuntimeFilterType::ExactSet,
        scope: treedb::query::runtime_filter::RuntimeFilterScope::Global,
        key_exprs: vec![Expr::Column("user_id".to_string())],
    });
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx).unwrap_err();
    assert!(matches!(
        err3,
        PlanValidationError::InvalidRuntimeFilter { .. }
    ));

    // 4. Merge join with unsorted left child
    let mut mutated4 = join_plan.clone();
    mutated4.joins[0].strategy = JoinStrategy::Merge;
    mutated4.joins[0].left_sort = false; // requires left ordered by u.id, but scan is unordered
    mutated4.base_table.access_path = AccessPath::FullTableScan; // full scan on users gives id, but let's change join on email
    mutated4.joins[0].on = Expr::Binary {
        left: Box::new(Expr::Column("u.email".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("o.status".to_string())),
    };
    let err4 = PhysicalPlanValidator::validate(&mutated4, &val_ctx).unwrap_err();
    assert!(matches!(
        err4,
        PlanValidationError::OrderingViolation { .. }
    ));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 6. Aggregation & Window Function Violations
// ============================================================================

#[test]
fn test_aggregation_and_window_violations() -> Result<()> {
    let ctx = TestContext::new("agg_win_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;

    // 1. Invalid GROUP BY column
    let agg_plan = tx.plan_sql("SELECT age, COUNT(*) FROM users GROUP BY age;")?;
    let mut mutated = agg_plan.clone();
    mutated.group_by = vec![Expr::Column("invalid_col".to_string())];
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(err, PlanValidationError::MissingColumn { .. }));

    // 2. Invalid Aggregate argument
    let mut mutated2 = agg_plan.clone();
    mutated2.aggregates = vec![AggregateExpr::Function {
        func: AggregateFunction::Sum,
        expr: Expr::Column("missing_col".to_string()),
    }];
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(err2, PlanValidationError::MissingColumn { .. }));

    // 3. Invalid Window partition column
    let win_plan = tx.plan_sql(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY age ORDER BY score DESC) FROM users;",
    )?;
    let mut mutated3 = win_plan.clone();
    mutated3.window_exprs[0].window_spec.partition_by =
        vec![Expr::Column("unknown_col".to_string())];
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx).unwrap_err();
    assert!(matches!(err3, PlanValidationError::MissingColumn { .. }));

    // 4. Invalid Window order column
    let mut mutated4 = win_plan.clone();
    mutated4.window_exprs[0].window_spec.order_by[0].expression =
        Expr::Column("unknown_col".to_string());
    let err4 = PhysicalPlanValidator::validate(&mutated4, &val_ctx).unwrap_err();
    assert!(matches!(err4, PlanValidationError::MissingColumn { .. }));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 7. Set Operation & Materialization Violations
// ============================================================================

#[test]
fn test_set_op_and_materialization_violations() -> Result<()> {
    let ctx = TestContext::new("set_op_mat_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;

    // 1. Set operation column count mismatch
    let set_plan =
        tx.plan_sql("SELECT id, name FROM users UNION ALL SELECT id, name FROM users;")?;
    let mut mutated = set_plan.clone();
    if let Some(set_op) = &mut mutated.set_op {
        set_op
            .right
            .projections
            .push((Expr::Column("age".to_string()), None));
    }
    let err = PhysicalPlanValidator::validate(&mutated, &val_ctx).unwrap_err();
    assert!(matches!(
        err,
        PlanValidationError::InvalidSetOperation { .. }
    ));

    // 2. CTE schema mismatch
    let cte_plan = tx.plan_sql("WITH cte AS (SELECT id, name FROM users) SELECT * FROM cte;")?;
    let mut mutated2 = cte_plan.clone();
    mutated2.ctes[0].output_schema =
        Schema::new_projected(vec![Column::new("id", DataType::Int64, false)]); // declared 1 col, but inner plan has 2 cols
    let err2 = PhysicalPlanValidator::validate(&mutated2, &val_ctx).unwrap_err();
    assert!(matches!(
        err2,
        PlanValidationError::InvalidMaterialization { .. }
    ));

    // 3. Recursive CTE with zero iterations
    let mut mutated3 = cte_plan.clone();
    mutated3.ctes[0].kind = treedb::sql::BoundCteKind::Recursive {
        anchor_plan: cte_plan.clone(),
        recursive_plan: cte_plan.clone(),
        max_iterations: 0,
        max_rows: 100,
    };
    let err3 = PhysicalPlanValidator::validate(&mutated3, &val_ctx).unwrap_err();
    assert!(matches!(
        err3,
        PlanValidationError::InvalidMaterialization { .. }
    ));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 8. Resource Signature & Plan Cache Violations
// ============================================================================

#[test]
fn test_resource_and_cache_violations() -> Result<()> {
    let ctx = TestContext::new("res_cache_violations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;
    let plan = tx.plan_sql("SELECT * FROM users;")?;

    // 1. Zero memory budget
    let invalid_res_ctx = val_ctx
        .clone()
        .with_resource_signature(Some(ResourcePlanningSignature::new(0, 10, 1024 * 1024, 4)));
    let err = PhysicalPlanValidator::validate(&plan, &invalid_res_ctx).unwrap_err();
    assert!(matches!(
        err,
        PlanValidationError::InvalidResourceConfiguration { .. }
    ));

    // 2. Stale Catalog Generation
    let stale_ctx = val_ctx.clone().with_plan_validity(Some(PlanValidity::new(
        999, // active is different
        0,
        0,
        vec!["users".to_string()],
        vec![],
    )));
    let err2 = PhysicalPlanValidator::validate(&plan, &stale_ctx).unwrap_err();
    assert!(matches!(
        err2,
        PlanValidationError::InvalidPlanCacheEntry { .. }
    ));

    // 3. Dropped table in authoritative catalog
    let mut mutated_plan = plan.clone();
    mutated_plan.base_table.table_name = "dropped_table".to_string();
    let err3 = PhysicalPlanValidator::validate(&mutated_plan, &val_ctx).unwrap_err();
    assert!(matches!(
        err3,
        PlanValidationError::InvalidPlanCacheEntry { .. }
            | PlanValidationError::InvalidAccessPath { .. }
    ));

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 9. Plan Mutation Testing (Deliberate Mutation Matrix)
// ============================================================================

#[test]
fn test_plan_mutation_matrix() -> Result<()> {
    let ctx = TestContext::new("mutation_matrix");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;

    let plan = tx.plan_sql("SELECT id, name, age FROM users WHERE age > 20 ORDER BY age ASC;")?;

    // Mutation 1: Corrupt Table Name
    let mut m1 = plan.clone();
    m1.base_table.table_name = "phantom_table".to_string();
    assert!(PhysicalPlanValidator::validate(&m1, &val_ctx).is_err());

    // Mutation 2: Corrupt Required Columns
    let mut m2 = plan.clone();
    m2.base_table.required_columns.push("bad_col".to_string());
    assert!(PhysicalPlanValidator::validate(&m2, &val_ctx).is_err());

    // Mutation 3: Corrupt Provided Ordering
    let mut m3 = plan.clone();
    m3.provided_ordering = Some(OrderingProperty::new(vec![OrderingColumn {
        column: "email".to_string(),
        direction: SortDirection::Descending,
        nulls_first: true,
    }]));
    assert!(PhysicalPlanValidator::validate(&m3, &val_ctx).is_err());

    // Mutation 4: Corrupt Projection Dependency
    let mut m4 = plan.clone();
    m4.projections[0] = (Expr::Column("non_existent".to_string()), None);
    assert!(PhysicalPlanValidator::validate(&m4, &val_ctx).is_err());

    // Mutation 5: Corrupt Row Goal Candidate Rows
    let mut m5 = plan.clone();
    m5.limit = Some(5);
    m5.offset = Some(5);
    m5.row_goal = Some(RowGoal::new(999, 5)); // Mismatch with LIMIT 5 + OFFSET 5 = 10
    assert!(PhysicalPlanValidator::validate(&m5, &val_ctx).is_err());

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 10. Randomized Plan Mutation Testing
// ============================================================================

#[test]
fn test_randomized_plan_mutations() -> Result<()> {
    let ctx = TestContext::new("randomized_mutations");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let val_ctx = tx.create_validation_context()?;

    let queries = [
        "SELECT * FROM users;",
        "SELECT id, name FROM users WHERE age > 25;",
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
        "SELECT age, COUNT(*) FROM users GROUP BY age;",
        "SELECT id, score, ROW_NUMBER() OVER (ORDER BY score DESC) FROM users;",
    ];

    for (q_idx, sql) in queries.iter().enumerate() {
        let valid_plan = tx.plan_sql(sql)?;

        // 1. Verify uncorrupted plan is accepted (zero false positives)
        let res = PhysicalPlanValidator::validate(&valid_plan, &val_ctx);
        assert!(
            res.is_ok(),
            "Original plan for '{sql}' must pass validation"
        );

        // 2. Perform 5 deterministic mutations per query and verify rejection
        for m in 0..5 {
            let mut corrupted = valid_plan.clone();
            match (q_idx + m) % 5 {
                0 => {
                    // Corrupt column
                    corrupted
                        .base_table
                        .required_columns
                        .push(format!("corrupt_col_{m}"));
                }
                1 => {
                    // Corrupt ordering
                    corrupted.provided_ordering =
                        Some(OrderingProperty::new(vec![OrderingColumn {
                            column: "unindexed_nonexistent".to_string(),
                            direction: SortDirection::Descending,
                            nulls_first: false,
                        }]));
                }
                2 => {
                    // Corrupt projection
                    corrupted
                        .projections
                        .push((Expr::Column(format!("phantom_{m}")), None));
                }
                3 => {
                    // Corrupt table
                    corrupted.base_table.table_name = format!("table_404_{m}");
                }
                4 => {
                    // Corrupt row goal
                    corrupted.row_goal = Some(RowGoal::new(2, 10)); // < 10
                }
                _ => unreachable!(),
            }

            let rejected = PhysicalPlanValidator::validate(&corrupted, &val_ctx);
            assert!(
                rejected.is_err(),
                "Mutated plan variant {m} for '{sql}' must be rejected by validator"
            );
        }
    }

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 11. Differential Execution Tests
// ============================================================================

#[test]
fn test_differential_execution_equivalence() -> Result<()> {
    let ctx = TestContext::new("diff_exec");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;

    let test_queries = vec![
        "SELECT id, name FROM users WHERE age > 20 ORDER BY id ASC;",
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name ASC;",
        "SELECT age, COUNT(*) FROM users GROUP BY age ORDER BY age ASC;",
    ];

    for sql in test_queries {
        let plan = tx.plan_sql(sql)?;
        let validated = tx.validate_physical_plan(&plan)?;

        // Execute via standard execute_sql
        let res_std = tx.execute_sql(sql)?;
        // Execute via validated physical plan
        let res_val = tx.execute_validated_physical_plan(&validated)?;

        assert_eq!(res_std.schema.columns.len(), res_val.schema.columns.len());
        assert_eq!(res_std.rows.len(), res_val.rows.len());
        for (r1, r2) in res_std.rows.iter().zip(res_val.rows.iter()) {
            assert_eq!(r1.values, r2.values);
        }
    }

    tx.commit()?;
    Ok(())
}

// ============================================================================
// 12. Persistence / Recovery Tests
// ============================================================================

#[test]
fn test_persistence_and_recovery() -> Result<()> {
    let ctx = TestContext::new("persist_recovery");
    {
        let mut db = ctx.db();
        setup_test_schema(&mut db)?;
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT * FROM users WHERE id = 1;")?;
        assert_eq!(res.rows.len(), 1);
        tx.commit()?;
        db.checkpoint()?;
    }

    // Reopen database from disk
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let plan =
            tx.plan_sql("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;")?;
        let validated = tx.validate_physical_plan(&plan);
        assert!(
            validated.is_ok(),
            "Plan must validate after reopening database"
        );
        let res = tx.execute_validated_physical_plan(&validated.unwrap())?;
        assert_eq!(res.rows.len(), 3);
        tx.commit()?;
    }

    Ok(())
}

// ============================================================================
// 13. EXPLAIN Diagnostics Formatting
// ============================================================================

#[test]
fn test_explain_validation_diagnostics() -> Result<()> {
    let ctx = TestContext::new("explain_diag");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut tx = db.begin_transaction()?;

    // 1. Valid plan produces "Validation: OK"
    let explain = tx.explain_sql("SELECT * FROM users WHERE age > 25;")?;
    assert!(
        explain.contains("Validation: OK"),
        "Valid EXPLAIN output must include 'Validation: OK'"
    );

    // 2. Invalid plan diagnostics format
    let val_ctx = tx.create_validation_context()?;
    let valid_plan = tx.plan_sql("SELECT * FROM users;")?;
    let mut invalid_plan = valid_plan.clone();
    invalid_plan
        .base_table
        .required_columns
        .push("missing_col".to_string());

    let diag = PhysicalPlanValidator::format_validation_diagnostics(&invalid_plan, &val_ctx);
    assert!(diag.contains("Plan validation failed") || diag.contains("missing_col"));

    tx.commit()?;
    Ok(())
}
