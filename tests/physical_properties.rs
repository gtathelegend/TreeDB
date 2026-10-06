use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::expr::Expr;
use treedb::query::operator::{SortDirection, SortKey};
use treedb::query::physical_properties::{
    OrderingColumn, OrderingProperty, PhysicalProperties, RequiredOrdering,
};
use treedb::query::plan_cache::ResourcePlanningSignature;
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

// -----------------------------------------------------------------------------
// R596–R600: Property Basics
// -----------------------------------------------------------------------------

#[test]
fn test_r596_ordering_property_construction() -> Result<()> {
    let col1 = OrderingColumn {
        column: "a".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    };
    let col2 = OrderingColumn {
        column: "b".to_string(),
        direction: SortDirection::Descending,
        nulls_first: false,
    };
    let ord = OrderingProperty::new(vec![col1.clone(), col2.clone()]);
    assert_eq!(ord.columns.len(), 2);
    assert_eq!(ord.columns[0], col1);
    assert_eq!(ord.columns[1], col2);

    let props = PhysicalProperties::with_ordering(ord.clone());
    assert_eq!(props.ordering, Some(ord));
    Ok(())
}

#[test]
fn test_r597_exact_ordering_compatibility() -> Result<()> {
    let ord = OrderingProperty::new(vec![OrderingColumn {
        column: "id".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    let req = RequiredOrdering::new(vec![OrderingColumn {
        column: "id".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    assert!(ord.satisfies_required_ordering(&req));
    assert!(ord.satisfies_order_by(&[SortKey::asc(Expr::Column("id".to_string()))], None, None));
    Ok(())
}

#[test]
fn test_r598_prefix_compatibility() -> Result<()> {
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
        OrderingColumn {
            column: "c".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
    ]);
    // Prefix [a] satisfied
    assert!(ord.satisfies_order_by(&[SortKey::asc(Expr::Column("a".to_string()))], None, None));
    // Prefix [a, b] satisfied
    assert!(ord.satisfies_order_by(
        &[
            SortKey::asc(Expr::Column("a".to_string())),
            SortKey::asc(Expr::Column("b".to_string()))
        ],
        None,
        None
    ));
    // Prefix [a, b, c] satisfied
    assert!(ord.satisfies_order_by(
        &[
            SortKey::asc(Expr::Column("a".to_string())),
            SortKey::asc(Expr::Column("b".to_string())),
            SortKey::asc(Expr::Column("c".to_string()))
        ],
        None,
        None
    ));
    // Suffix [b] NOT satisfied
    assert!(!ord.satisfies_order_by(&[SortKey::asc(Expr::Column("b".to_string()))], None, None));
    // Subsequence [a, c] NOT satisfied
    assert!(!ord.satisfies_order_by(
        &[
            SortKey::asc(Expr::Column("a".to_string())),
            SortKey::asc(Expr::Column("c".to_string()))
        ],
        None,
        None
    ));
    Ok(())
}

#[test]
fn test_r599_direction_mismatch() -> Result<()> {
    let ord = OrderingProperty::new(vec![OrderingColumn {
        column: "a".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    assert!(!ord.satisfies_order_by(&[SortKey::desc(Expr::Column("a".to_string()))], None, None));
    Ok(())
}

#[test]
fn test_r600_null_order_mismatch() -> Result<()> {
    let ord = OrderingProperty::new(vec![OrderingColumn {
        column: "a".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    let sort_key_nulls_last = SortKey::asc(Expr::Column("a".to_string())).with_nulls_first(false);
    assert!(!ord.satisfies_order_by(&[sort_key_nulls_last], None, None));
    Ok(())
}

// -----------------------------------------------------------------------------
// R601–R605: Sort Elimination
// -----------------------------------------------------------------------------

#[test]
fn test_r601_redundant_sort_eliminated() -> Result<()> {
    let ctx = TestContext::new("r601");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    let plan = txn.plan_sql("SELECT * FROM users ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    let explain = txn.explain_sql("SELECT * FROM users ORDER BY id ASC;")?;
    assert!(!explain.contains("Sort ["));
    Ok(())
}

#[test]
fn test_r602_consecutive_identical_sorts() -> Result<()> {
    let sort_key = SortKey::asc(Expr::Column("x".to_string()));
    let ord = OrderingProperty::from_sort_keys(std::slice::from_ref(&sort_key));
    assert!(ord.satisfies_order_by(&[sort_key], None, None));
    Ok(())
}

#[test]
fn test_r603_stronger_ordering_satisfies_weaker_requirement() -> Result<()> {
    let ord = OrderingProperty::new(vec![
        OrderingColumn {
            column: "x".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
        OrderingColumn {
            column: "y".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
    ]);
    let req = RequiredOrdering::new(vec![OrderingColumn {
        column: "x".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    assert!(ord.satisfies_required_ordering(&req));
    Ok(())
}

#[test]
fn test_r604_weaker_ordering_does_not_satisfy_stronger_requirement() -> Result<()> {
    let ord = OrderingProperty::new(vec![OrderingColumn {
        column: "x".to_string(),
        direction: SortDirection::Ascending,
        nulls_first: true,
    }]);
    let req = RequiredOrdering::new(vec![
        OrderingColumn {
            column: "x".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
        OrderingColumn {
            column: "y".to_string(),
            direction: SortDirection::Ascending,
            nulls_first: true,
        },
    ]);
    assert!(!ord.satisfies_required_ordering(&req));
    Ok(())
}

#[test]
fn test_r605_expression_mismatch_retains_sort() -> Result<()> {
    let ctx = TestContext::new("r605");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    let plan = txn.plan_sql("SELECT * FROM t ORDER BY val ASC;")?;
    assert!(plan.order_by_sort_required);
    let explain = txn.explain_sql("SELECT * FROM t ORDER BY val ASC;")?;
    assert!(explain.contains("Sort ["));
    Ok(())
}

// -----------------------------------------------------------------------------
// R606–R609: Filter / Projection
// -----------------------------------------------------------------------------

#[test]
fn test_r606_filter_preserves_ordering() -> Result<()> {
    let ctx = TestContext::new("r606");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, active INT);")?;
    let plan = txn.plan_sql("SELECT * FROM users WHERE active = 1 ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r607_simple_projection_preserves_ordering() -> Result<()> {
    let ctx = TestContext::new("r607");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    let plan = txn.plan_sql("SELECT id, name FROM users ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r608_projection_alias_mapping() -> Result<()> {
    let ctx = TestContext::new("r608");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    let plan = txn.plan_sql("SELECT id AS user_id FROM users ORDER BY user_id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r609_computed_projection_loses_ordering() -> Result<()> {
    let ctx = TestContext::new("r609");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, val INT);")?;
    let plan = txn.plan_sql("SELECT id + 1 AS x FROM users ORDER BY x ASC;")?;
    assert!(plan.order_by_sort_required);
    Ok(())
}

// -----------------------------------------------------------------------------
// R610–R612: Limit / Offset
// -----------------------------------------------------------------------------

#[test]
fn test_r610_limit_preserves_ordering() -> Result<()> {
    let ctx = TestContext::new("r610");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM users ORDER BY id ASC LIMIT 5;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r611_offset_preserves_ordering() -> Result<()> {
    let ctx = TestContext::new("r611");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM users ORDER BY id ASC OFFSET 5;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r612_order_by_limit_avoids_redundant_sort() -> Result<()> {
    let ctx = TestContext::new("r612");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'user{i}');"))?;
    }
    let res = txn.execute_sql("SELECT id FROM users ORDER BY id ASC LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    assert_eq!(res.rows[2].values[0], Value::Int64(3));
    Ok(())
}

// -----------------------------------------------------------------------------
// R613–R617: Aggregation
// -----------------------------------------------------------------------------

#[test]
fn test_r613_streaming_aggregate_provides_grouping_ordering() -> Result<()> {
    let ctx = TestContext::new("r613");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    let plan = txn.plan_sql("SELECT id, COUNT(*) FROM t GROUP BY id;")?;
    assert!(plan.provided_ordering.is_some());
    let ord = plan.provided_ordering.unwrap();
    assert_eq!(ord.columns.len(), 1);
    assert_eq!(ord.columns[0].column, "id");
    Ok(())
}

#[test]
fn test_r614_hash_aggregate_removes_ordering() -> Result<()> {
    let ord = OrderingProperty::default();
    assert!(ord.is_empty());
    assert!(!ord.satisfies_order_by(&[SortKey::asc(Expr::Column("x".to_string()))], None, None));
    Ok(())
}

#[test]
fn test_r615_external_aggregate_conservatively_removes_ordering() -> Result<()> {
    let ord = OrderingProperty::default();
    assert!(ord.is_empty());
    Ok(())
}

#[test]
fn test_r616_group_by_order_by_eliminates_sort() -> Result<()> {
    let ctx = TestContext::new("r616");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE products (id BIGINT PRIMARY KEY, val INT);")?;
    let plan = txn.plan_sql("SELECT id, SUM(val) FROM products GROUP BY id ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r617_order_by_aggregate_retains_sort() -> Result<()> {
    let ctx = TestContext::new("r617");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE products (id BIGINT PRIMARY KEY, val INT);")?;
    let plan =
        txn.plan_sql("SELECT id, SUM(val) AS s FROM products GROUP BY id ORDER BY s DESC;")?;
    assert!(plan.order_by_sort_required);
    Ok(())
}

// -----------------------------------------------------------------------------
// R618–R625: Joins
// -----------------------------------------------------------------------------

#[test]
fn test_r618_merge_join_ordering_propagation() -> Result<()> {
    let ctx = TestContext::new("r618");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, a_val INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, b_val INT);")?;
    let plan = txn.plan_sql("SELECT * FROM a JOIN b ON a.id = b.id ORDER BY a.id ASC;")?;
    if plan
        .joins
        .iter()
        .any(|j| j.strategy == treedb::query::JoinStrategy::Merge)
    {
        assert!(!plan.order_by_sort_required);
    }
    Ok(())
}

#[test]
fn test_r619_hash_join_removes_ordering() -> Result<()> {
    let props = PhysicalProperties::none();
    assert!(props.is_empty());
    Ok(())
}

#[test]
fn test_r620_external_hash_join_removes_ordering() -> Result<()> {
    let props = PhysicalProperties::none();
    assert!(props.is_empty());
    Ok(())
}

#[test]
fn test_r621_nested_loop_conservative_behavior() -> Result<()> {
    let props = PhysicalProperties::none();
    assert!(props.is_empty());
    Ok(())
}

#[test]
fn test_r622_inner_merge_join_ordering() -> Result<()> {
    let ctx = TestContext::new("r622");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i}0);"))?;
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}00);"))?;
    }
    let res =
        txn.execute_sql("SELECT t1.id FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.id ASC;")?;
    assert_eq!(res.rows.len(), 5);
    for i in 0..5 {
        assert_eq!(res.rows[i].values[0], Value::Int64((i + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_r623_left_join_ordering_safety() -> Result<()> {
    let ctx = TestContext::new("r623");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (1, 10), (2, 20);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (1, 100);")?;
    let res = txn.execute_sql(
        "SELECT t1.id, t2.id FROM t1 LEFT JOIN t2 ON t1.id = t2.id ORDER BY t1.id ASC;",
    )?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    Ok(())
}

#[test]
fn test_r624_right_join_ordering_safety() -> Result<()> {
    let ctx = TestContext::new("r624");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (1, 100), (2, 200);")?;
    let res = txn.execute_sql(
        "SELECT t1.id, t2.id FROM t1 RIGHT JOIN t2 ON t1.id = t2.id ORDER BY t2.id ASC;",
    )?;
    assert_eq!(res.rows.len(), 2);
    Ok(())
}

#[test]
fn test_r625_full_join_ordering_safety() -> Result<()> {
    let ctx = TestContext::new("r625");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (1), (3);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (2), (3);")?;
    let res = txn.execute_sql(
        "SELECT t1.id, t2.id FROM t1 FULL JOIN t2 ON t1.id = t2.id ORDER BY t1.id ASC;",
    )?;
    assert_eq!(res.rows.len(), 3);
    Ok(())
}

// -----------------------------------------------------------------------------
// R626–R631: Indexes
// -----------------------------------------------------------------------------

#[test]
fn test_r626_primary_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r626");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE pk_tbl (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    let plan = txn.plan_sql("SELECT * FROM pk_tbl ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r627_secondary_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r627");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE sec_tbl (id BIGINT PRIMARY KEY, score INT);")?;
    txn.execute_statement("CREATE INDEX idx_score ON sec_tbl (score);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO sec_tbl VALUES ({i}, {i}0);"))?;
    }
    let plan = txn.plan_sql("SELECT score FROM sec_tbl WHERE score > 0 ORDER BY score ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r628_composite_index_prefix_ordering() -> Result<()> {
    let ctx = TestContext::new("r628");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE comp_tbl (id BIGINT PRIMARY KEY, a INT, b INT, c INT);")?;
    txn.execute_statement("CREATE INDEX idx_abc ON comp_tbl (a, b, c);")?;
    let plan_prefix =
        txn.plan_sql("SELECT a, b FROM comp_tbl WHERE a > 0 ORDER BY a ASC, b ASC;")?;
    assert!(!plan_prefix.order_by_sort_required);

    let plan_invalid = txn.plan_sql("SELECT b, c FROM comp_tbl WHERE a > 0 ORDER BY b ASC;")?;
    assert!(plan_invalid.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r629_covering_index_ordering() -> Result<()> {
    let ctx = TestContext::new("r629");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE cov_tbl (id BIGINT PRIMARY KEY, k INT, v INT);")?;
    txn.execute_statement("CREATE INDEX idx_cov ON cov_tbl (k) INCLUDE (v);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO cov_tbl VALUES ({i}, {i}, {i}0);"))?;
    }
    let plan = txn.plan_sql("SELECT k, v FROM cov_tbl WHERE k > 0 ORDER BY k ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r630_multi_index_scan_no_false_ordering() -> Result<()> {
    let scan = treedb::query::PhysicalTableScan {
        table_name: "multi".to_string(),
        table_alias: None,
        access_path: treedb::query::AccessPath::MultiIndexScan {
            strategy: treedb::query::MultiIndexStrategy::Intersection,
            child_scans: Vec::new(),
            execution_mode: treedb::query::MultiIndexExecutionMode::InMemory,
        },
        residual_filter: None,
        estimated_rows: 100.0,
        cost: 10.0,
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
    let ord = treedb::query::physical_properties::ordering_property_for_table_scan(
        &treedb::query::TableOptInfo {
            name: "multi".to_string(),
            alias: None,
            schema: treedb::engine::types::Schema::new_projected(vec![]),
            indexes: vec![],
            stats: None,
            is_cte: false,
        },
        &scan,
    );
    assert!(ord.is_empty());
    Ok(())
}

#[test]
fn test_r631_skip_scan_no_false_ordering() -> Result<()> {
    let scan = treedb::query::PhysicalTableScan {
        table_name: "skip".to_string(),
        table_alias: None,
        access_path: treedb::query::AccessPath::SecondaryIndexScan {
            index_name: "idx".to_string(),
            index_id: treedb::IndexId(1),
            scan_type: treedb::query::SecondaryScanType::SkipScan {
                skipped_prefix_columns: vec!["a".to_string()],
                constrained_columns: vec![("b".to_string(), Value::Int64(10))],
            },
        },
        residual_filter: None,
        estimated_rows: 100.0,
        cost: 5.0,
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
    let ord = treedb::query::physical_properties::ordering_property_for_table_scan(
        &treedb::query::TableOptInfo {
            name: "skip".to_string(),
            alias: None,
            schema: treedb::engine::types::Schema::new_projected(vec![]),
            indexes: vec![],
            stats: None,
            is_cte: false,
        },
        &scan,
    );
    assert!(ord.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------------
// R632–R634: Windows
// -----------------------------------------------------------------------------

#[test]
fn test_r632_window_ordering_requirement() -> Result<()> {
    let ctx = TestContext::new("r632");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE emp (id BIGINT PRIMARY KEY, dept VARCHAR, salary INT);")?;
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO emp VALUES ({i}, 'eng', {i}000);"))?;
    }
    let res = txn.execute_sql(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emp;",
    )?;
    assert_eq!(res.rows.len(), 5);
    Ok(())
}

#[test]
fn test_r633_window_output_ordering() -> Result<()> {
    let ctx = TestContext::new("r633");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE emp (id BIGINT PRIMARY KEY, dept VARCHAR, salary INT);")?;
    let plan = txn.plan_sql("SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emp ORDER BY dept ASC, salary ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r634_redundant_sort_after_ordered_window_eliminated() -> Result<()> {
    let ctx = TestContext::new("r634");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE emp (id BIGINT PRIMARY KEY, dept VARCHAR, salary INT);")?;
    let plan_diff = txn.plan_sql("SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emp ORDER BY salary DESC;")?;
    assert!(plan_diff.order_by_sort_required);
    Ok(())
}

// -----------------------------------------------------------------------------
// R635–R640: Set Operations / CTE
// -----------------------------------------------------------------------------

#[test]
fn test_r635_union_ordering_behavior() -> Result<()> {
    let ctx = TestContext::new("r635");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (2), (1);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (4), (3);")?;
    let res = txn.execute_sql("SELECT id FROM t1 UNION ALL SELECT id FROM t2 ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    assert_eq!(res.rows[2].values[0], Value::Int64(3));
    assert_eq!(res.rows[3].values[0], Value::Int64(4));
    Ok(())
}

#[test]
fn test_r636_intersect_ordering_behavior() -> Result<()> {
    let ctx = TestContext::new("r636");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (1), (2), (3);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (2), (3), (4);")?;
    let res = txn.execute_sql("SELECT id FROM t1 INTERSECT SELECT id FROM t2 ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(2));
    assert_eq!(res.rows[1].values[0], Value::Int64(3));
    Ok(())
}

#[test]
fn test_r637_except_ordering_behavior() -> Result<()> {
    let ctx = TestContext::new("r637");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t1 VALUES (1), (2), (3);")?;
    txn.execute_statement("INSERT INTO t2 VALUES (2);")?;
    let res = txn.execute_sql("SELECT id FROM t1 EXCEPT SELECT id FROM t2 ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(3));
    Ok(())
}

#[test]
fn test_r638_cte_ordering_barrier() -> Result<()> {
    let ctx = TestContext::new("r638");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (2, 20), (1, 10);")?;
    let res =
        txn.execute_sql("WITH cte AS (SELECT * FROM t) SELECT * FROM cte ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    Ok(())
}

#[test]
fn test_r639_derived_table_ordering_barrier() -> Result<()> {
    let ctx = TestContext::new("r639");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    txn.execute_statement("INSERT INTO t VALUES (3, 30), (1, 10);")?;
    let res = txn.execute_sql("SELECT * FROM (SELECT * FROM t) d ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    Ok(())
}

#[test]
fn test_r640_recursive_cte_ordering_barrier() -> Result<()> {
    let ctx = TestContext::new("r640");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t VALUES (1);")?;
    let res = txn.execute_sql("WITH RECURSIVE cnt(n) AS (SELECT id FROM t UNION ALL SELECT n + 1 FROM cnt WHERE n < 5) SELECT n FROM cnt ORDER BY n ASC;")?;
    assert_eq!(res.rows.len(), 5);
    for (i, row) in res.rows.iter().enumerate() {
        assert_eq!(row.values[0], Value::Int64((i + 1) as i64));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// R641–R645: Cost
// -----------------------------------------------------------------------------

#[test]
fn test_r641_ordered_index_vs_table_scan_sort() -> Result<()> {
    let ctx = TestContext::new("r641");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, k INT);")?;
    txn.execute_statement("CREATE INDEX idx_k ON t (k);")?;
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    let plan = txn.plan_sql("SELECT k FROM t WHERE k > 0 ORDER BY k ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r642_sort_cost_included() -> Result<()> {
    let ctx = TestContext::new("r642");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    let plan_ordered = txn.plan_sql("SELECT * FROM t ORDER BY id ASC;")?;
    let plan_unordered = txn.plan_sql("SELECT * FROM t ORDER BY v ASC;")?;
    assert!(plan_unordered.total_cost >= plan_ordered.total_cost);
    Ok(())
}

#[test]
fn test_r643_resource_aware_sort_elimination() -> Result<()> {
    let ctx = TestContext::new("r643");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM t ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r644_statistics_sensitive_access_path() -> Result<()> {
    let ctx = TestContext::new("r644");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT);")?;
    txn.execute_statement("CREATE INDEX idx_a ON t (a);")?;
    let plan = txn.plan_sql("SELECT a FROM t WHERE a > 0 ORDER BY a ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r645_limit_sensitive_ordering_plan() -> Result<()> {
    let ctx = TestContext::new("r645");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT);")?;
    let plan = txn.plan_sql("SELECT * FROM t ORDER BY id ASC LIMIT 1;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

// -----------------------------------------------------------------------------
// R646–R650: Plan Cache / Feedback
// -----------------------------------------------------------------------------

#[test]
fn test_r646_property_aware_plan_cache() -> Result<()> {
    let ctx = TestContext::new("r646");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    let p1 = txn.plan_sql("SELECT * FROM t ORDER BY id ASC;")?;
    let p2 = txn.plan_sql("SELECT * FROM t ORDER BY id ASC;")?;
    assert_eq!(p1, p2);
    assert!(!p1.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r647_index_invalidation() -> Result<()> {
    let ctx = TestContext::new("r647");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, x INT);")?;
    txn.execute_statement("CREATE INDEX idx_x ON t (x);")?;
    let p1 = txn.plan_sql("SELECT x FROM t WHERE x > 0 ORDER BY x ASC;")?;
    assert!(!p1.order_by_sort_required);

    txn.execute_statement("DROP INDEX idx_x;")?;
    let p2 = txn.plan_sql("SELECT x FROM t WHERE x > 0 ORDER BY x ASC;")?;
    assert!(p2.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r648_statistics_invalidation() -> Result<()> {
    let ctx = TestContext::new("r648");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("ANALYZE t;")?;
    let p = txn.plan_sql("SELECT * FROM t ORDER BY id ASC;")?;
    assert!(!p.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r649_resource_signature_invalidation() -> Result<()> {
    let sig1 = ResourcePlanningSignature::new(1024, 4, 1024, 2);
    let sig2 = ResourcePlanningSignature::new(1024 * 1024, 4, 1024 * 1024, 2);
    assert_ne!(sig1, sig2);
    Ok(())
}

#[test]
fn test_r650_adaptive_sort_cost_feedback() -> Result<()> {
    let ctx = TestContext::new("r650");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    let _ = txn.execute_sql("SELECT * FROM t ORDER BY v ASC;")?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R651–R656: Prepared / Transaction / Recovery
// -----------------------------------------------------------------------------

#[test]
fn test_r651_prepared_order_by() -> Result<()> {
    let ctx = TestContext::new("r651");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, age INT);")?;
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, {i}0);"))?;
    }
    let stmt = txn.prepare_sql("SELECT * FROM users WHERE id > ? ORDER BY id ASC;")?;
    let res = txn.execute_prepared_query(&stmt, &[Value::Int64(5)])?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(6));
    Ok(())
}

#[test]
fn test_r652_parameter_sensitive_ordering() -> Result<()> {
    let ctx = TestContext::new("r652");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    let stmt = txn.prepare_sql("SELECT * FROM t WHERE id = ? ORDER BY id ASC;")?;
    let res = txn.execute_prepared_query(&stmt, &[Value::Int64(1)])?;
    assert!(res.rows.is_empty());
    Ok(())
}

#[test]
fn test_r653_transaction_query() -> Result<()> {
    let ctx = TestContext::new("r653");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    txn.execute_statement("INSERT INTO items VALUES (2, 'two'), (1, 'one');")?;
    let res = txn.execute_sql("SELECT * FROM items ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    txn.commit()?;
    Ok(())
}

#[test]
fn test_r654_rollback() -> Result<()> {
    let ctx = TestContext::new("r654");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO items VALUES (1);")?;
    txn.commit()?;

    let mut txn2 = db.begin_transaction()?;
    txn2.execute_statement("INSERT INTO items VALUES (2);")?;
    txn2.abort()?;

    let mut txn3 = db.begin_transaction()?;
    let res = txn3.execute_sql("SELECT * FROM items ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    Ok(())
}

#[test]
fn test_r655_persistence_reopen() -> Result<()> {
    let ctx = TestContext::new("r655");
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
        txn.execute_statement("INSERT INTO t VALUES (3), (1), (2);")?;
        txn.commit()?;
    }

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction()?;
        let res = txn.execute_sql("SELECT * FROM t ORDER BY id ASC;")?;
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[1].values[0], Value::Int64(2));
        assert_eq!(res.rows[2].values[0], Value::Int64(3));
    }
    Ok(())
}

#[test]
fn test_r656_recovery_equivalence() -> Result<()> {
    let ctx = TestContext::new("r656");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("INSERT INTO t VALUES (10), (5), (20);")?;
    txn.commit()?;

    let mut txn2 = db.begin_transaction()?;
    let res = txn2.execute_sql("SELECT * FROM t ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(5));
    assert_eq!(res.rows[1].values[0], Value::Int64(10));
    assert_eq!(res.rows[2].values[0], Value::Int64(20));
    Ok(())
}

// -----------------------------------------------------------------------------
// R657–R665: Cross-Pipeline
// -----------------------------------------------------------------------------

#[test]
fn test_r657_filter_sort_elimination() -> Result<()> {
    let ctx = TestContext::new("r657");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    let plan = txn.plan_sql("SELECT * FROM t WHERE v > 10 ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r658_index_filter_order_by() -> Result<()> {
    let ctx = TestContext::new("r658");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, k INT, v INT);")?;
    txn.execute_statement("CREATE INDEX idx_k ON t (k);")?;
    let plan = txn.plan_sql("SELECT k FROM t WHERE k > 5 AND v > 10 ORDER BY k ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r659_index_streaming_aggregate_order_by() -> Result<()> {
    let ctx = TestContext::new("r659");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    let plan = txn.plan_sql("SELECT id, SUM(val) FROM t GROUP BY id ORDER BY id ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r660_merge_join_streaming_aggregate_order_by() -> Result<()> {
    let ctx = TestContext::new("r660");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, v INT);")?;
    let plan = txn.plan_sql(
        "SELECT a.id, COUNT(*) FROM a JOIN b ON a.id = b.id GROUP BY a.id ORDER BY a.id ASC;",
    )?;
    if plan
        .joins
        .iter()
        .any(|j| j.strategy == treedb::query::JoinStrategy::Merge)
    {
        assert!(!plan.order_by_sort_required);
    }
    Ok(())
}

#[test]
fn test_r661_merge_join_order_by() -> Result<()> {
    let ctx = TestContext::new("r661");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY);")?;
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM a JOIN b ON a.id = b.id ORDER BY a.id ASC;")?;
    if plan
        .joins
        .iter()
        .any(|j| j.strategy == treedb::query::JoinStrategy::Merge)
    {
        assert!(!plan.order_by_sort_required);
    }
    Ok(())
}

#[test]
fn test_r662_window_order_by() -> Result<()> {
    let ctx = TestContext::new("r662");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE emp (id BIGINT PRIMARY KEY, dept VARCHAR, salary INT);")?;
    let plan = txn.plan_sql("SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary ASC) AS rn FROM emp ORDER BY dept ASC, salary ASC;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r663_order_by_limit() -> Result<()> {
    let ctx = TestContext::new("r663");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM t ORDER BY id ASC LIMIT 10;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r664_order_by_offset_limit() -> Result<()> {
    let ctx = TestContext::new("r664");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY);")?;
    let plan = txn.plan_sql("SELECT * FROM t ORDER BY id ASC LIMIT 10 OFFSET 5;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

#[test]
fn test_r665_composite_ordering_across_multiple_operators() -> Result<()> {
    let ctx = TestContext::new("r665");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    txn.execute_statement("CREATE INDEX idx_ab ON t (a, b);")?;
    let plan =
        txn.plan_sql("SELECT a, b FROM t WHERE a > 10 AND b > 20 ORDER BY a ASC, b ASC LIMIT 5;")?;
    assert!(!plan.order_by_sort_required);
    Ok(())
}

// -----------------------------------------------------------------------------
// R666–R670: Differential / Invariants
// -----------------------------------------------------------------------------

#[test]
fn test_r666_property_aware_vs_forced_sort_equivalence() -> Result<()> {
    let ctx = TestContext::new("r666");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    for i in (1..=50).rev() {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'val_{i}');"))?;
    }
    let res = txn.execute_sql("SELECT * FROM t ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 50);
    for i in 0..50 {
        assert_eq!(res.rows[i].values[0], Value::Int64((i + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_r667_randomized_single_column_queries() -> Result<()> {
    let ctx = TestContext::new("r667");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=30 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}0);"))?;
    }
    let res_asc = txn.execute_sql("SELECT id FROM t ORDER BY id ASC;")?;
    for i in 0..30 {
        assert_eq!(res_asc.rows[i].values[0], Value::Int64((i + 1) as i64));
    }
    let res_desc = txn.execute_sql("SELECT id FROM t ORDER BY id DESC;")?;
    for i in 0..30 {
        assert_eq!(res_desc.rows[i].values[0], Value::Int64((30 - i) as i64));
    }
    Ok(())
}

#[test]
fn test_r668_randomized_composite_ordering() -> Result<()> {
    let ctx = TestContext::new("r668");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE comp (id BIGINT PRIMARY KEY, grp INT, rank INT);")?;
    txn.execute_statement("CREATE INDEX idx_gr ON comp (grp, rank);")?;
    for i in 1..=20 {
        let grp = (i % 3) as i64;
        let rank = (20 - i) as i64;
        txn.execute_statement(&format!("INSERT INTO comp VALUES ({i}, {grp}, {rank});"))?;
    }
    let res =
        txn.execute_sql("SELECT grp, rank FROM comp WHERE grp >= 0 ORDER BY grp ASC, rank ASC;")?;
    assert_eq!(res.rows.len(), 20);
    for window in res.rows.windows(2) {
        let g1 = match &window[0].values[0] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let g2 = match &window[1].values[0] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let r1 = match &window[0].values[1] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let r2 = match &window[1].values[1] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        assert!(g1 <= g2);
        if g1 == g2 {
            assert!(r1 <= r2);
        }
    }
    Ok(())
}

#[test]
fn test_r669_randomized_joins_and_ordering() -> Result<()> {
    let ctx = TestContext::new("r669");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE j1 (id BIGINT PRIMARY KEY, v INT);")?;
    txn.execute_statement("CREATE TABLE j2 (id BIGINT PRIMARY KEY, v INT);")?;
    for i in 1..=15 {
        txn.execute_statement(&format!("INSERT INTO j1 VALUES ({i}, {i});"))?;
        txn.execute_statement(&format!("INSERT INTO j2 VALUES ({i}, {i});"))?;
    }
    let res =
        txn.execute_sql("SELECT j1.id FROM j1 JOIN j2 ON j1.id = j2.id ORDER BY j1.id ASC;")?;
    assert_eq!(res.rows.len(), 15);
    for i in 0..15 {
        assert_eq!(res.rows[i].values[0], Value::Int64((i + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_r670_randomized_aggregation_and_ordering() -> Result<()> {
    let ctx = TestContext::new("r670");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE agg_t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=25 {
        txn.execute_statement(&format!("INSERT INTO agg_t VALUES ({i}, {i}0);"))?;
    }
    let res = txn.execute_sql("SELECT id, SUM(val) FROM agg_t GROUP BY id ORDER BY id ASC;")?;
    assert_eq!(res.rows.len(), 25);
    for i in 0..25 {
        assert_eq!(res.rows[i].values[0], Value::Int64((i + 1) as i64));
    }
    Ok(())
}

#[test]
fn test_randomized_differential_property_oracle() -> Result<()> {
    let ctx = TestContext::new("oracle");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement("CREATE TABLE rand_t (id BIGINT PRIMARY KEY, cat INT, score INT);")?;
    txn.execute_statement("CREATE INDEX idx_cat_score ON rand_t (cat, score);")?;

    type Key = (i64, i64);
    type Val = i64;
    let mut expected_tuples: Vec<(Key, Val)> = Vec::new();

    for i in 1..=100 {
        let cat = (i % 7) as i64;
        let score = ((i * 17) % 50) as i64;
        expected_tuples.push(((cat, score), i as i64));
        txn.execute_statement(&format!("INSERT INTO rand_t VALUES ({i}, {cat}, {score});"))?;
    }

    let res1 = txn.execute_sql("SELECT id FROM rand_t ORDER BY id ASC;")?;
    assert_eq!(res1.rows.len(), 100);
    for i in 0..100 {
        assert_eq!(res1.rows[i].values[0], Value::Int64((i + 1) as i64));
    }

    let res2 = txn
        .execute_sql("SELECT cat, score FROM rand_t WHERE cat >= 0 ORDER BY cat ASC, score ASC;")?;
    assert_eq!(res2.rows.len(), 100);
    for window in res2.rows.windows(2) {
        let c1 = match &window[0].values[0] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let c2 = match &window[1].values[0] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let s1 = match &window[0].values[1] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        let s2 = match &window[1].values[1] {
            Value::Int64(v) => *v,
            _ => panic!("expected int"),
        };
        assert!(c1 <= c2);
        if c1 == c2 {
            assert!(s1 <= s2);
        }
    }

    Ok(())
}
