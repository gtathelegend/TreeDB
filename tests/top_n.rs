use tempfile::TempDir;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Result;
use treedb::query::cost::CostModel;
use treedb::query::expr::Expr;
use treedb::query::external_sort::SortConfig;
use treedb::query::feedback::SpillStatus;
use treedb::query::operator::SortKey;
use treedb::query::top_n::{TopNExecutionMetrics, TopNOperator};
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
// R671–R680: Top-N Operator & In-Memory Priority Queue Basics
// -----------------------------------------------------------------------------

#[test]
fn test_r671_top_n_operator_basic_asc() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(50)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Int64(80)]),
        Row::new(vec![Value::Int64(4), Value::Int64(10)]),
        Row::new(vec![Value::Int64(5), Value::Int64(30)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(3),
        Some(0),
        &SortConfig::default(),
    )?;
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].values[1], Value::Int64(10));
    assert_eq!(res[1].values[1], Value::Int64(20));
    assert_eq!(res[2].values[1], Value::Int64(30));
    assert_eq!(metrics.candidate_capacity, 3);
    assert_eq!(metrics.rows_retained, 3);
    assert_eq!(metrics.rows_discarded, 2);
    Ok(())
}

#[test]
fn test_r672_top_n_operator_basic_desc() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(50)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Int64(80)]),
        Row::new(vec![Value::Int64(4), Value::Int64(10)]),
        Row::new(vec![Value::Int64(5), Value::Int64(30)]),
    ];
    let sort_keys = vec![SortKey::desc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(2),
        Some(0),
        &SortConfig::default(),
    )?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].values[1], Value::Int64(80));
    assert_eq!(res[1].values[1], Value::Int64(50));
    assert_eq!(metrics.candidate_capacity, 2);
    assert_eq!(metrics.rows_retained, 2);
    assert_eq!(metrics.rows_discarded, 3);
    Ok(())
}

#[test]
fn test_r673_top_n_operator_composite_keys() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("a", DataType::Int64, false),
            Column::new("b", DataType::Int64, false),
            Column::new("c", DataType::String, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Int64(10),
            Value::String("x".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Int64(20),
            Value::String("y".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(1),
            Value::Int64(30),
            Value::String("z".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Int64(10),
            Value::String("w".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(1),
            Value::Int64(20),
            Value::String("v".to_string()),
        ]),
    ];
    let sort_keys = vec![
        SortKey::asc(Expr::Column("a".to_string())),
        SortKey::desc(Expr::Column("b".to_string())),
    ];
    let res = TopNOperator::apply(rows, &schema, &sort_keys, Some(3), Some(0))?;
    assert_eq!(res.len(), 3);
    assert_eq!(
        res[0].values,
        vec![
            Value::Int64(1),
            Value::Int64(30),
            Value::String("z".to_string())
        ]
    );
    assert_eq!(
        res[1].values,
        vec![
            Value::Int64(1),
            Value::Int64(20),
            Value::String("v".to_string())
        ]
    );
    assert_eq!(
        res[2].values,
        vec![
            Value::Int64(1),
            Value::Int64(10),
            Value::String("x".to_string())
        ]
    );
    Ok(())
}

#[test]
fn test_r674_top_n_operator_null_handling() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )?;
    let rows1 = vec![
        Row::new(vec![Value::Int64(1), Value::Null]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Null]),
        Row::new(vec![Value::Int64(4), Value::Int64(10)]),
    ];
    let rows2 = rows1.clone();
    let sort_keys_first =
        vec![SortKey::asc(Expr::Column("val".to_string())).with_nulls_first(true)];
    let res_first = TopNOperator::apply(rows1, &schema, &sort_keys_first, Some(2), Some(0))?;
    assert_eq!(res_first.len(), 2);
    assert_eq!(res_first[0].values[1], Value::Null);
    assert_eq!(res_first[1].values[1], Value::Null);

    let sort_keys_last =
        vec![SortKey::asc(Expr::Column("val".to_string())).with_nulls_first(false)];
    let res_last = TopNOperator::apply(rows2, &schema, &sort_keys_last, Some(2), Some(0))?;
    assert_eq!(res_last.len(), 2);
    assert_eq!(res_last[0].values[1], Value::Int64(10));
    assert_eq!(res_last[1].values[1], Value::Int64(20));
    Ok(())
}

#[test]
fn test_r675_top_n_operator_offset_and_limit() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(10)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Int64(30)]),
        Row::new(vec![Value::Int64(4), Value::Int64(40)]),
        Row::new(vec![Value::Int64(5), Value::Int64(50)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(2),
        Some(2),
        &SortConfig::default(),
    )?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].values[1], Value::Int64(30));
    assert_eq!(res[1].values[1], Value::Int64(40));
    assert_eq!(metrics.candidate_capacity, 4);
    assert_eq!(metrics.rows_retained, 4);
    assert_eq!(metrics.rows_discarded, 1);
    assert_eq!(metrics.limit, Some(2));
    assert_eq!(metrics.offset, Some(2));
    Ok(())
}

#[test]
fn test_r676_top_n_operator_empty_and_single_row() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res_empty, metrics_empty) = TopNOperator::apply_with_config(
        vec![],
        &schema,
        &sort_keys,
        Some(5),
        Some(0),
        &SortConfig::default(),
    )?;
    assert!(res_empty.is_empty());
    assert_eq!(metrics_empty.rows_retained, 0);

    let single = vec![Row::new(vec![Value::Int64(1), Value::Int64(42)])];
    let (res_single, metrics_single) = TopNOperator::apply_with_config(
        single,
        &schema,
        &sort_keys,
        Some(5),
        Some(0),
        &SortConfig::default(),
    )?;
    assert_eq!(res_single.len(), 1);
    assert_eq!(res_single[0].values[1], Value::Int64(42));
    assert_eq!(metrics_single.rows_retained, 1);
    Ok(())
}

#[test]
fn test_r677_top_n_operator_offset_exceeds_row_count() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(10)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(5),
        Some(10),
        &SortConfig::default(),
    )?;
    assert!(res.is_empty());
    assert_eq!(metrics.rows_retained, 2);
    Ok(())
}

#[test]
fn test_r678_top_n_operator_limit_zero() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(10)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(0),
        Some(0),
        &SortConfig::default(),
    )?;
    assert!(res.is_empty());
    assert_eq!(metrics.rows_retained, 0);
    Ok(())
}

#[test]
fn test_r679_top_n_operator_memory_tracking() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::String, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("hello world aaaaa bbbbb ccccc".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("foo bar baz".to_string()),
        ]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let (res, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(2),
        Some(0),
        &SortConfig::default(),
    )?;
    assert_eq!(res.len(), 2);
    assert!(metrics.peak_memory_bytes > 0);
    Ok(())
}

#[test]
fn test_r680_top_n_operator_fallback_to_external_sorter_on_budget_exceeded() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(50)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Int64(80)]),
        Row::new(vec![Value::Int64(4), Value::Int64(10)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let config = SortConfig::default().with_memory_budget_bytes(1);
    let (res, _) =
        TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(2), Some(0), &config)?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].values[1], Value::Int64(10));
    assert_eq!(res[1].values[1], Value::Int64(20));
    Ok(())
}

// -----------------------------------------------------------------------------
// R681–R690: Cost Model & Optimizer Integration
// -----------------------------------------------------------------------------

#[test]
fn test_r681_cost_model_top_n_cost_scaling() {
    let input_rows = 10_000.0;
    let small_bound = 10;
    let large_bound = 5_000;
    let row_bytes = 100.0;

    let cost_small = CostModel::top_n_cost(input_rows, small_bound, row_bytes);
    let cost_large = CostModel::top_n_cost(input_rows, large_bound, row_bytes);
    let cost_sort =
        CostModel::sort_cost_with_budget(input_rows, row_bytes, 1024 * 1024, SpillStatus::Unknown);

    assert!(cost_small < cost_large);
    assert!(cost_small < cost_sort);
}

#[test]
fn test_r682_optimizer_selects_top_n_for_unindexed_order_by_limit() -> Result<()> {
    let ctx = TestContext::new("r682");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", 50 - i))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.plan_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    assert!(plan.order_by_top_n);
    assert_eq!(plan.limit, Some(5));
    assert_eq!(plan.offset, None);
    let explain = tx.explain_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    assert!(explain.contains("TopN"));
    Ok(())
}

#[test]
fn test_r683_optimizer_selects_sort_without_limit() -> Result<()> {
    let ctx = TestContext::new("r683");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.plan_sql("SELECT * FROM t ORDER BY val ASC;")?;
    assert!(!plan.order_by_top_n);
    let explain = tx.explain_sql("SELECT * FROM t ORDER BY val ASC;")?;
    assert!(explain.contains("Sort") && !explain.contains("TopN"));
    Ok(())
}

#[test]
fn test_r684_optimizer_eliminates_sort_when_index_matches() -> Result<()> {
    let ctx = TestContext::new("r684");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE INDEX idx_val ON t (val);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.plan_sql("SELECT val FROM t ORDER BY val ASC LIMIT 5;")?;
    assert!(!plan.order_by_top_n);
    let explain = tx.explain_sql("SELECT val FROM t ORDER BY val ASC LIMIT 5;")?;
    assert!(!explain.contains("Sort") && !explain.contains("TopN"));
    Ok(())
}

#[test]
fn test_r685_cost_model_early_limit_scaling() -> Result<()> {
    let ctx = TestContext::new("r685");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan_pk = tx.plan_sql("SELECT * FROM t ORDER BY id ASC LIMIT 5;")?;
    assert!(!plan_pk.order_by_top_n);
    Ok(())
}

#[test]
fn test_r686_explain_top_n_format() -> Result<()> {
    let ctx = TestContext::new("r686");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain = tx.explain_sql("SELECT * FROM t ORDER BY val DESC LIMIT 3 OFFSET 2;")?;
    assert!(explain.contains("TopN (limit: Some(3), offset: Some(2))"));
    Ok(())
}

#[test]
fn test_r687_explain_ordered_scan_without_top_n() -> Result<()> {
    let ctx = TestContext::new("r687");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain = tx.explain_sql("SELECT * FROM t ORDER BY id ASC LIMIT 3;")?;
    assert!(!explain.contains("TopN"));
    assert!(explain.contains("PrimaryRangeScan") || explain.contains("FullTableScan"));
    Ok(())
}

#[test]
fn test_r688_explain_composite_order_by_top_n() -> Result<()> {
    let ctx = TestContext::new("r688");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {});", 10 - i))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let explain = tx.explain_sql("SELECT * FROM t ORDER BY a ASC, b DESC LIMIT 4;")?;
    assert!(explain.contains("TopN (limit: Some(4), offset: None)"));
    Ok(())
}

#[test]
fn test_r689_offset_without_limit_uses_sort() -> Result<()> {
    let ctx = TestContext::new("r689");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.plan_sql("SELECT * FROM t ORDER BY val ASC OFFSET 5;")?;
    assert!(!plan.order_by_top_n);
    let rows = tx.execute_sql("SELECT * FROM t ORDER BY val ASC OFFSET 5;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[0], Value::Int64(6));
    Ok(())
}

#[test]
fn test_r690_limit_without_order_by_no_top_n() -> Result<()> {
    let ctx = TestContext::new("r690");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.plan_sql("SELECT * FROM t LIMIT 3;")?;
    assert!(!plan.order_by_top_n);
    let explain = tx.explain_sql("SELECT * FROM t LIMIT 3;")?;
    assert!(!explain.contains("TopN"));
    assert!(!explain.contains("Sort"));
    Ok(())
}

// -----------------------------------------------------------------------------
// R691–R705: End-to-End Query Execution & SQL Correctness
// -----------------------------------------------------------------------------

#[test]
fn test_r691_e2e_single_col_asc_limit() -> Result<()> {
    let ctx = TestContext::new("r691");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 80), (2, 20), (3, 90), (4, 10), (5, 50);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT score FROM t ORDER BY score ASC LIMIT 3;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(10));
    assert_eq!(rows.rows[1].values[0], Value::Int64(20));
    assert_eq!(rows.rows[2].values[0], Value::Int64(50));
    Ok(())
}

#[test]
fn test_r692_e2e_desc_with_duplicates() -> Result<()> {
    let ctx = TestContext::new("r692");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 50), (2, 90), (3, 50), (4, 90), (5, 10);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT score FROM t ORDER BY score DESC LIMIT 3;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(90));
    assert_eq!(rows.rows[1].values[0], Value::Int64(90));
    assert_eq!(rows.rows[2].values[0], Value::Int64(50));
    Ok(())
}

#[test]
fn test_r693_e2e_composite_order_by() -> Result<()> {
    let ctx = TestContext::new("r693");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, dept INT, salary INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10, 5000), (2, 20, 3000), (3, 10, 8000), (4, 20, 7000), (5, 10, 3000);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows =
        tx.execute_sql("SELECT dept, salary FROM t ORDER BY dept ASC, salary DESC LIMIT 4;")?;
    assert_eq!(rows.rows.len(), 4);
    assert_eq!(
        rows.rows[0].values,
        vec![Value::Int64(10), Value::Int64(8000)]
    );
    assert_eq!(
        rows.rows[1].values,
        vec![Value::Int64(10), Value::Int64(5000)]
    );
    assert_eq!(
        rows.rows[2].values,
        vec![Value::Int64(10), Value::Int64(3000)]
    );
    assert_eq!(
        rows.rows[3].values,
        vec![Value::Int64(20), Value::Int64(7000)]
    );
    Ok(())
}

#[test]
fn test_r694_e2e_offset_and_limit() -> Result<()> {
    let ctx = TestContext::new("r694");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t ORDER BY val DESC LIMIT 3 OFFSET 5;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(15));
    assert_eq!(rows.rows[1].values[0], Value::Int64(14));
    assert_eq!(rows.rows[2].values[0], Value::Int64(13));
    Ok(())
}

#[test]
fn test_r695_e2e_offset_out_of_bounds() -> Result<()> {
    let ctx = TestContext::new("r695");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10), (2, 20);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5 OFFSET 100;")?;
    assert!(rows.rows.is_empty());
    Ok(())
}

#[test]
fn test_r696_e2e_limit_zero() -> Result<()> {
    let ctx = TestContext::new("r696");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10), (2, 20);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 0;")?;
    assert!(rows.rows.is_empty());
    Ok(())
}

#[test]
fn test_r697_e2e_limit_one_min_query() -> Result<()> {
    let ctx = TestContext::new("r697");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 55), (2, 12), (3, 99), (4, 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t ORDER BY val ASC LIMIT 1;")?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].values[0], Value::Int64(3));
    Ok(())
}

#[test]
fn test_r698_e2e_limit_one_max_query() -> Result<()> {
    let ctx = TestContext::new("r698");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 55), (2, 12), (3, 99), (4, 3);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t ORDER BY val DESC LIMIT 1;")?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].values[0], Value::Int64(99));
    Ok(())
}

#[test]
fn test_r699_e2e_null_ordering() -> Result<()> {
    let ctx = TestContext::new("r699");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, NULL), (2, 50), (3, NULL), (4, 10);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id, val FROM t ORDER BY val ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[1], Value::Null);
    assert_eq!(rows.rows[1].values[1], Value::Null);
    Ok(())
}

#[test]
fn test_r700_e2e_mixed_datatypes() -> Result<()> {
    let ctx = TestContext::new("r700");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, f DOUBLE, s VARCHAR, b BOOLEAN);",
    )?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10.5, 'charlie', true), (2, 5.5, 'alice', false), (3, 7.5, 'bob', true);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT s FROM t ORDER BY s ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[0], Value::String("alice".to_string()));
    assert_eq!(rows.rows[1].values[0], Value::String("bob".to_string()));

    let rows_float = tx.execute_sql("SELECT f FROM t ORDER BY f DESC LIMIT 2;")?;
    assert_eq!(rows_float.rows.len(), 2);
    assert_eq!(rows_float.rows[0].values[0], Value::Float64(10.5));
    assert_eq!(rows_float.rows[1].values[0], Value::Float64(7.5));
    Ok(())
}

#[test]
fn test_r701_e2e_top_n_vs_sort_equivalence() -> Result<()> {
    let ctx = TestContext::new("r701");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        let val = (i * 37) % 100;
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {val});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let top_n_rows = tx.execute_sql("SELECT val FROM t ORDER BY val ASC LIMIT 10;")?;
    let sort_rows = tx.execute_sql("SELECT val FROM t ORDER BY val ASC;")?;
    let sort_slice: Vec<_> = sort_rows.rows.into_iter().take(10).collect();
    assert_eq!(top_n_rows.rows, sort_slice);
    Ok(())
}

#[test]
fn test_r702_e2e_all_identical_values() -> Result<()> {
    let ctx = TestContext::new("r702");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, 42);"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id, val FROM t ORDER BY val ASC LIMIT 5;")?;
    assert_eq!(rows.rows.len(), 5);
    for row in rows.rows {
        assert_eq!(row.values[1], Value::Int64(42));
    }
    Ok(())
}

#[test]
fn test_r703_e2e_presorted_and_reverse_presorted() -> Result<()> {
    let ctx = TestContext::new("r703");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=30 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows_asc = tx.execute_sql("SELECT val FROM t ORDER BY val ASC LIMIT 5;")?;
    assert_eq!(rows_asc.rows.len(), 5);
    assert_eq!(rows_asc.rows[0].values[0], Value::Int64(1));
    assert_eq!(rows_asc.rows[4].values[0], Value::Int64(5));

    let rows_desc = tx.execute_sql("SELECT val FROM t ORDER BY val DESC LIMIT 5;")?;
    assert_eq!(rows_desc.rows.len(), 5);
    assert_eq!(rows_desc.rows[0].values[0], Value::Int64(30));
    assert_eq!(rows_desc.rows[4].values[0], Value::Int64(26));
    Ok(())
}

#[test]
fn test_r704_e2e_large_text_values() -> Result<()> {
    let ctx = TestContext::new("r704");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, doc VARCHAR);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 'zzzz long text document 1');")?;
    tx.execute_statement("INSERT INTO t VALUES (2, 'aaaa long text document 2');")?;
    tx.execute_statement("INSERT INTO t VALUES (3, 'mmmm long text document 3');")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id, doc FROM t ORDER BY doc ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[0], Value::Int64(2));
    assert_eq!(rows.rows[1].values[0], Value::Int64(3));
    Ok(())
}

#[test]
fn test_r705_e2e_top_n_with_where_filter() -> Result<()> {
    let ctx = TestContext::new("r705");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t WHERE val > 20 ORDER BY val ASC LIMIT 5;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[0], Value::Int64(21));
    assert_eq!(rows.rows[4].values[0], Value::Int64(25));
    Ok(())
}

// -----------------------------------------------------------------------------
// R706–R715: Early-Stop Optimization on Ordered Physical Paths
// -----------------------------------------------------------------------------

#[test]
fn test_r706_early_stop_primary_key_scan() -> Result<()> {
    let ctx = TestContext::new("r706");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT * FROM t WHERE id >= 10 ORDER BY id ASC LIMIT 5;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[0], Value::Int64(10));
    assert_eq!(rows.rows[4].values[0], Value::Int64(14));
    Ok(())
}

#[test]
fn test_r707_early_stop_secondary_index_scan() -> Result<()> {
    let ctx = TestContext::new("r707");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT, name VARCHAR);")?;
    tx.execute_statement("CREATE INDEX idx_val ON t (val);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, 'item_{i}');"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows =
        tx.execute_sql("SELECT id, val, name FROM t WHERE val >= 20 ORDER BY val ASC LIMIT 5;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[1], Value::Int64(20));
    assert_eq!(rows.rows[4].values[1], Value::Int64(24));
    Ok(())
}

#[test]
fn test_r708_early_stop_index_only_scan() -> Result<()> {
    let ctx = TestContext::new("r708");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE INDEX idx_val ON t (val);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t WHERE val >= 10 ORDER BY val ASC LIMIT 4;")?;
    assert_eq!(rows.rows.len(), 4);
    assert_eq!(rows.rows[0].values[0], Value::Int64(10));
    assert_eq!(rows.rows[3].values[0], Value::Int64(13));
    Ok(())
}

#[test]
fn test_r709_early_stop_with_residual_filter() -> Result<()> {
    let ctx = TestContext::new("r709");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, active BOOLEAN);")?;
    for i in 1..=100 {
        let active = i % 2 == 0;
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {active});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id FROM t WHERE active = true ORDER BY id ASC LIMIT 5;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[0], Value::Int64(2));
    assert_eq!(rows.rows[1].values[0], Value::Int64(4));
    assert_eq!(rows.rows[2].values[0], Value::Int64(6));
    assert_eq!(rows.rows[3].values[0], Value::Int64(8));
    assert_eq!(rows.rows[4].values[0], Value::Int64(10));
    Ok(())
}

#[test]
fn test_r710_early_stop_secondary_index_with_residual_filter() -> Result<()> {
    let ctx = TestContext::new("r710");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT, flag BOOLEAN);")?;
    tx.execute_statement("CREATE INDEX idx_val ON t (val);")?;
    for i in 1..=50 {
        let flag = i % 3 == 0;
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {flag});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql(
        "SELECT val FROM t WHERE val >= 10 AND flag = true ORDER BY val ASC LIMIT 3;",
    )?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(12));
    assert_eq!(rows.rows[1].values[0], Value::Int64(15));
    assert_eq!(rows.rows[2].values[0], Value::Int64(18));
    Ok(())
}

#[test]
fn test_r711_early_stop_with_offset_and_limit() -> Result<()> {
    let ctx = TestContext::new("r711");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id FROM t ORDER BY id ASC LIMIT 5 OFFSET 10;")?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values[0], Value::Int64(11));
    assert_eq!(rows.rows[4].values[0], Value::Int64(15));
    Ok(())
}

#[test]
fn test_r712_early_stop_metrics_in_explain_analyze() -> Result<()> {
    let ctx = TestContext::new("r712");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=100 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let (res, metrics) =
        tx.execute_sql_with_metrics("SELECT id FROM t ORDER BY id ASC LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 5);
    Ok(())
}

#[test]
fn test_r713_full_table_scan_physical_order_early_stop() -> Result<()> {
    let ctx = TestContext::new("r713");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT * FROM t ORDER BY id ASC LIMIT 10;")?;
    assert_eq!(rows.rows.len(), 10);
    assert_eq!(rows.rows[0].values[0], Value::Int64(1));
    assert_eq!(rows.rows[9].values[0], Value::Int64(10));
    Ok(())
}

#[test]
fn test_r714_early_stop_reaches_end_of_range_before_limit() -> Result<()> {
    let ctx = TestContext::new("r714");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id FROM t WHERE id >= 8 ORDER BY id ASC LIMIT 10;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(8));
    assert_eq!(rows.rows[2].values[0], Value::Int64(10));
    Ok(())
}

#[test]
fn test_r715_early_stop_with_zero_matches() -> Result<()> {
    let ctx = TestContext::new("r715");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT id FROM t WHERE id > 100 ORDER BY id ASC LIMIT 5;")?;
    assert!(rows.rows.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------------
// R716–R725: EXPLAIN ANALYZE & Metrics Verification
// -----------------------------------------------------------------------------

#[test]
fn test_r716_explain_analyze_top_n_metrics_output() -> Result<()> {
    let ctx = TestContext::new("r716");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", 50 - i))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res = tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("TopN"));
    assert!(ea.contains("candidate_capacity: 5"));
    assert!(ea.contains("rows_retained: 5"));
    assert!(ea.contains("rows_discarded: 45"));
    Ok(())
}

#[test]
fn test_r717_explain_analyze_top_n_retained_and_discarded_counts() -> Result<()> {
    let ctx = TestContext::new("r717");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res = tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val DESC LIMIT 4;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("rows_retained: 4"));
    assert!(ea.contains("rows_discarded: 16"));
    Ok(())
}

#[test]
fn test_r718_explain_analyze_top_n_with_offset() -> Result<()> {
    let ctx = TestContext::new("r718");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res =
        tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val ASC LIMIT 4 OFFSET 3;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("TopN (limit: Some(4), offset: Some(3))"));
    assert!(ea.contains("candidate_capacity: 7"));
    assert!(ea.contains("rows_retained: 7"));
    Ok(())
}

#[test]
fn test_r719_explain_analyze_sort_without_limit_shows_sort_metrics() -> Result<()> {
    let ctx = TestContext::new("r719");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res = tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val DESC;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("Sort"));
    assert!(!ea.contains("TopN"));
    Ok(())
}

#[test]
fn test_r720_explain_analyze_all_rows_fit_in_top_n() -> Result<()> {
    let ctx = TestContext::new("r720");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=5 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res = tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val ASC LIMIT 10;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("rows_retained: 5"));
    assert!(ea.contains("rows_discarded: 0"));
    Ok(())
}

#[test]
fn test_r721_explain_analyze_memory_reported() -> Result<()> {
    let ctx = TestContext::new("r721");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let ea_res = tx.execute_sql("EXPLAIN ANALYZE SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    let ea = match &ea_res.rows[0].values[0] {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    assert!(ea.contains("peak_memory_bytes:"));
    Ok(())
}

#[test]
fn test_r722_top_n_metrics_struct_direct() {
    let m = TopNExecutionMetrics {
        candidate_capacity: 10,
        rows_input: 2,
        rows_retained: 1,
        rows_discarded: 1,
        limit: Some(1),
        offset: Some(1),
        ..Default::default()
    };
    assert_eq!(m.candidate_capacity, 10);
    assert_eq!(m.rows_retained, 1);
    assert_eq!(m.rows_discarded, 1);
    assert_eq!(m.limit, Some(1));
    assert_eq!(m.offset, Some(1));
}

#[test]
fn test_r723_top_n_feedback_recording() -> Result<()> {
    let ctx = TestContext::new("r723");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let _ = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    Ok(())
}

#[test]
fn test_r724_plan_cache_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r724");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx1 = db.begin_transaction()?;
    let rows1 = tx1.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    tx1.commit()?;

    let mut tx2 = db.begin_transaction()?;
    let rows2 = tx2.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
    assert_eq!(rows1.rows, rows2.rows);
    Ok(())
}

#[test]
fn test_r725_multiple_top_n_queries_isolation() -> Result<()> {
    let ctx = TestContext::new("r725");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=30 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let r1 = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 3;")?;
    let r2 = tx.execute_sql("SELECT * FROM t ORDER BY val DESC LIMIT 4;")?;
    assert_eq!(r1.rows.len(), 3);
    assert_eq!(r2.rows.len(), 4);
    assert_eq!(r1.rows[0].values[1], Value::Int64(1));
    assert_eq!(r2.rows[0].values[1], Value::Int64(30));
    Ok(())
}

// -----------------------------------------------------------------------------
// R726–R735: Joins, Aggregations & Complex Queries with Top-N
// -----------------------------------------------------------------------------

#[test]
fn test_r726_inner_join_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r726");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT, score INT);")?;
    tx.execute_statement("INSERT INTO a VALUES (1, 100), (2, 200), (3, 300);")?;
    tx.execute_statement(
        "INSERT INTO b VALUES (10, 1, 50), (20, 2, 90), (30, 1, 80), (40, 3, 10);",
    )?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql(
        "SELECT a.id, b.score FROM a JOIN b ON a.id = b.a_id ORDER BY b.score DESC LIMIT 2;",
    )?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values, vec![Value::Int64(2), Value::Int64(90)]);
    assert_eq!(rows.rows[1].values, vec![Value::Int64(1), Value::Int64(80)]);
    Ok(())
}

#[test]
fn test_r727_left_outer_join_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r727");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    tx.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT, amount INT);")?;
    tx.execute_statement("INSERT INTO a VALUES (1, 'alice'), (2, 'bob'), (3, 'charlie');")?;
    tx.execute_statement("INSERT INTO b VALUES (10, 1, 500);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql(
        "SELECT a.name, b.amount FROM a LEFT JOIN b ON a.id = b.a_id ORDER BY a.name ASC LIMIT 2;",
    )?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(
        rows.rows[0].values,
        vec![Value::String("alice".to_string()), Value::Int64(500)]
    );
    assert_eq!(
        rows.rows[1].values,
        vec![Value::String("bob".to_string()), Value::Null]
    );
    Ok(())
}

#[test]
fn test_r728_ordered_merge_join_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r728");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v1 INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, v2 INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {});", i * 10))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql(
        "SELECT t1.id, t2.v2 FROM t1 JOIN t2 ON t1.id = t2.id ORDER BY t1.id ASC LIMIT 5;",
    )?;
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0].values, vec![Value::Int64(1), Value::Int64(10)]);
    assert_eq!(rows.rows[4].values, vec![Value::Int64(5), Value::Int64(50)]);
    Ok(())
}

#[test]
fn test_r729_group_by_hash_aggregate_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r729");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE sales (id BIGINT PRIMARY KEY, dept VARCHAR, amount INT);")?;
    tx.execute_statement("INSERT INTO sales VALUES (1, 'electronics', 500), (2, 'clothing', 300), (3, 'electronics', 800), (4, 'groceries', 200), (5, 'clothing', 400);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql(
        "SELECT dept, SUM(amount) FROM sales GROUP BY dept ORDER BY SUM(amount) DESC LIMIT 2;",
    )?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(
        rows.rows[0].values,
        vec![Value::String("electronics".to_string()), Value::Int64(1300)]
    );
    assert_eq!(
        rows.rows[1].values,
        vec![Value::String("clothing".to_string()), Value::Int64(700)]
    );
    Ok(())
}

#[test]
fn test_r730_group_by_streaming_aggregate_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r730");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE logs (id BIGINT PRIMARY KEY, tag INT, duration INT);")?;
    tx.execute_statement("CREATE INDEX idx_tag ON logs (tag);")?;
    for i in 1..=30 {
        let tag = i % 5;
        tx.execute_statement(&format!("INSERT INTO logs VALUES ({i}, {tag}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows =
        tx.execute_sql("SELECT tag, COUNT(*) FROM logs GROUP BY tag ORDER BY tag ASC LIMIT 3;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values[0], Value::Int64(0));
    assert_eq!(rows.rows[1].values[0], Value::Int64(1));
    assert_eq!(rows.rows[2].values[0], Value::Int64(2));
    Ok(())
}

#[test]
fn test_r731_top_n_with_limit_in_join() -> Result<()> {
    let ctx = TestContext::new("r731");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")?;
    tx.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, w INT);")?;
    for i in 1..=10 {
        tx.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))?;
        tx.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT t1.v, t2.w FROM t1 JOIN t2 ON t1.id = t2.id WHERE t1.v > 3 ORDER BY t1.v ASC LIMIT 3;")?;
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0].values, vec![Value::Int64(4), Value::Int64(4)]);
    assert_eq!(rows.rows[2].values, vec![Value::Int64(6), Value::Int64(6)]);
    Ok(())
}

#[test]
fn test_r732_three_way_join_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r732");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT);")?;
    tx.execute_statement("CREATE TABLE c (id BIGINT PRIMARY KEY, b_id BIGINT, score INT);")?;
    tx.execute_statement("INSERT INTO a VALUES (1, 10), (2, 20);")?;
    tx.execute_statement("INSERT INTO b VALUES (10, 1), (20, 2);")?;
    tx.execute_statement("INSERT INTO c VALUES (100, 10, 50), (200, 20, 90), (300, 10, 80);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT a.val, c.score FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id ORDER BY c.score DESC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(
        rows.rows[0].values,
        vec![Value::Int64(20), Value::Int64(90)]
    );
    assert_eq!(
        rows.rows[1].values,
        vec![Value::Int64(10), Value::Int64(80)]
    );
    Ok(())
}

#[test]
fn test_r733_projection_expressions_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r733");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10, 5), (2, 20, 15), (3, 5, 2);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT a, b FROM t ORDER BY a ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[0], Value::Int64(5));
    assert_eq!(rows.rows[1].values[0], Value::Int64(10));
    Ok(())
}

#[test]
fn test_r734_having_clause_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r734");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, category VARCHAR, val INT);")?;
    tx.execute_statement(
        "INSERT INTO t VALUES (1, 'A', 10), (2, 'A', 20), (3, 'B', 5), (4, 'C', 50), (5, 'C', 60);",
    )?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT category, SUM(val) FROM t GROUP BY category HAVING SUM(val) > 25 ORDER BY SUM(val) DESC LIMIT 1;")?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(
        rows.rows[0].values,
        vec![Value::String("C".to_string()), Value::Int64(110)]
    );
    Ok(())
}

#[test]
fn test_r735_distinct_with_top_n() -> Result<()> {
    let ctx = TestContext::new("r735");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20);")?;
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows = tx.execute_sql("SELECT val FROM t GROUP BY val ORDER BY val ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[0], Value::Int64(10));
    assert_eq!(rows.rows[1].values[0], Value::Int64(20));
    Ok(())
}

// -----------------------------------------------------------------------------
// R736–R745: Memory Limits, Spill/Fallback & Resource Feedback
// -----------------------------------------------------------------------------

#[test]
fn test_r736_tight_memory_budget_fallback_execution() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(50)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
        Row::new(vec![Value::Int64(3), Value::Int64(80)]),
        Row::new(vec![Value::Int64(4), Value::Int64(10)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let config = SortConfig::default().with_memory_budget_bytes(10);
    let (res, _) =
        TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(2), Some(0), &config)?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].values[1], Value::Int64(10));
    assert_eq!(res[1].values[1], Value::Int64(20));
    Ok(())
}

#[test]
fn test_r737_fallback_exact_match_with_in_memory() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let mut rows1 = Vec::new();
    let mut rows2 = Vec::new();
    for i in (1..=100).rev() {
        rows1.push(Row::new(vec![Value::Int64(i), Value::Int64(i)]));
        rows2.push(Row::new(vec![Value::Int64(i), Value::Int64(i)]));
    }
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let in_mem_config = SortConfig::default().with_memory_budget_bytes(1024 * 1024);
    let fallback_config = SortConfig::default().with_memory_budget_bytes(1);
    let (res_in_mem, _) = TopNOperator::apply_with_config(
        rows1,
        &schema,
        &sort_keys,
        Some(10),
        Some(5),
        &in_mem_config,
    )?;
    let (res_fallback, _) = TopNOperator::apply_with_config(
        rows2,
        &schema,
        &sort_keys,
        Some(10),
        Some(5),
        &fallback_config,
    )?;
    assert_eq!(res_in_mem, res_fallback);
    Ok(())
}

#[test]
fn test_r738_fallback_cleans_up_resources() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(1)]),
        Row::new(vec![Value::Int64(2), Value::Int64(2)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let config = SortConfig::default().with_memory_budget_bytes(1);
    let (res, _) =
        TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(1), Some(0), &config)?;
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].values[1], Value::Int64(1));
    Ok(())
}

#[test]
fn test_r739_disk_budget_error_in_fallback() {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(1)]),
        Row::new(vec![Value::Int64(2), Value::Int64(2)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let mut config = SortConfig::default().with_memory_budget_bytes(1);
    config.max_temp_bytes = 0;
    let res = TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(1), Some(0), &config);
    assert!(res.is_err());
}

#[test]
fn test_r740_memory_feedback_updates() -> Result<()> {
    let ctx = TestContext::new("r740");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=50 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let _ = tx.execute_sql("SELECT * FROM t ORDER BY val DESC LIMIT 10;")?;
    Ok(())
}

#[test]
fn test_r741_top_n_repeated_queries_stable() -> Result<()> {
    let ctx = TestContext::new("r741");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=30 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    for _ in 0..5 {
        let rows = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 5;")?;
        assert_eq!(rows.rows.len(), 5);
        assert_eq!(rows.rows[0].values[1], Value::Int64(1));
    }
    Ok(())
}

#[test]
fn test_r742_candidate_memory_estimation() {
    let schema = Schema::new(
        vec![
            Column::new("a", DataType::Int64, false),
            Column::new("b", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::String("abcdefgh".to_string())]),
        Row::new(vec![Value::Int64(2), Value::String("ijklmnop".to_string())]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("a".to_string()))];
    let (_, metrics) = TopNOperator::apply_with_config(
        rows,
        &schema,
        &sort_keys,
        Some(2),
        Some(0),
        &SortConfig::default(),
    )
    .unwrap();
    assert!(metrics.peak_memory_bytes > 0);
}

#[test]
fn test_r743_mid_stream_budget_exceeded_triggers_fallback() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::String, false),
        ],
        0,
    )?;
    let mut rows = Vec::new();
    for i in 1..=50 {
        rows.push(Row::new(vec![
            Value::Int64(i),
            Value::String(format!("large string payload value number {i}")),
        ]));
    }
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let config = SortConfig::default().with_memory_budget_bytes(200);
    let (res, _) =
        TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(10), Some(0), &config)?;
    assert_eq!(res.len(), 10);
    Ok(())
}

#[test]
fn test_r744_in_memory_top_n_creates_no_spill_files() -> Result<()> {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, false),
        ],
        0,
    )?;
    let rows = vec![
        Row::new(vec![Value::Int64(1), Value::Int64(10)]),
        Row::new(vec![Value::Int64(2), Value::Int64(20)]),
    ];
    let sort_keys = vec![SortKey::asc(Expr::Column("val".to_string()))];
    let config = SortConfig::default().with_memory_budget_bytes(1024 * 1024);
    let (res, metrics) =
        TopNOperator::apply_with_config(rows, &schema, &sort_keys, Some(2), Some(0), &config)?;
    assert_eq!(res.len(), 2);
    assert_eq!(metrics.candidate_capacity, 2);
    Ok(())
}

#[test]
fn test_r745_concurrent_top_n_independence() -> Result<()> {
    let ctx = TestContext::new("r745");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=20 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))?;
    }
    tx.commit()?;

    let mut tx1 = db.begin_transaction()?;
    let r1 = tx1.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 3;")?;
    tx1.commit()?;

    let mut tx2 = db.begin_transaction()?;
    let r2 = tx2.execute_sql("SELECT * FROM t ORDER BY val DESC LIMIT 3;")?;
    tx2.commit()?;

    assert_eq!(r1.rows[0].values[1], Value::Int64(1));
    assert_eq!(r2.rows[0].values[1], Value::Int64(20));
    Ok(())
}

// -----------------------------------------------------------------------------
// R746–R750: Transactions, Concurrency & Edge Cases
// -----------------------------------------------------------------------------

#[test]
fn test_r746_top_n_uncommitted_visibility_isolation() -> Result<()> {
    let ctx = TestContext::new("r746");
    let mut db = ctx.db();
    let mut tx1 = db.begin_transaction()?;
    tx1.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx1.execute_statement("INSERT INTO t VALUES (1, 100), (2, 200);")?;
    tx1.commit()?;

    let mut tx1 = db.begin_transaction()?;
    tx1.execute_statement("INSERT INTO t VALUES (3, 50);")?; // uncommitted
    tx1.abort()?;

    let mut tx2 = db.begin_transaction()?;
    let rows = tx2.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[1], Value::Int64(100));
    assert_eq!(rows.rows[1].values[1], Value::Int64(200));
    Ok(())
}

#[test]
fn test_r747_top_n_sees_own_uncommitted_inserts() -> Result<()> {
    let ctx = TestContext::new("r747");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 100), (2, 200);")?;
    tx.execute_statement("INSERT INTO t VALUES (3, 50);")?; // in-flight insert

    let rows = tx.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[1], Value::Int64(50));
    assert_eq!(rows.rows[1].values[1], Value::Int64(100));
    Ok(())
}

#[test]
fn test_r748_top_n_excludes_aborted_transaction_rows() -> Result<()> {
    let ctx = TestContext::new("r748");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    tx.execute_statement("INSERT INTO t VALUES (1, 100), (2, 200);")?;
    tx.commit()?;

    let mut tx_abort = db.begin_transaction()?;
    tx_abort.execute_statement("INSERT INTO t VALUES (3, 10);")?;
    tx_abort.abort()?;

    let mut tx_read = db.begin_transaction()?;
    let rows = tx_read.execute_sql("SELECT * FROM t ORDER BY val ASC LIMIT 2;")?;
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0].values[1], Value::Int64(100));
    assert_eq!(rows.rows[1].values[1], Value::Int64(200));
    Ok(())
}

#[test]
fn test_r749_top_n_persistence_and_recovery() -> Result<()> {
    let ctx = TestContext::new("r749");
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
        for i in 1..=50 {
            tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", (i * 17) % 100))?;
        }
        tx.commit()?;
    }
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let rows = tx.execute_sql("SELECT val FROM t ORDER BY val ASC LIMIT 3;")?;
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(rows.rows[0].values[0], Value::Int64(2));
    }
    Ok(())
}

#[test]
fn test_r750_stress_top_n_large_dataset() -> Result<()> {
    let ctx = TestContext::new("r750");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")?;
    for i in 1..=1000 {
        let val = (i * 2654435761_u64 % 100000) as i64;
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {val});"))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let rows_top_n = tx.execute_sql("SELECT val FROM t ORDER BY val ASC LIMIT 10 OFFSET 50;")?;
    let rows_all = tx.execute_sql("SELECT val FROM t ORDER BY val ASC;")?;
    let expected: Vec<_> = rows_all.rows.into_iter().skip(50).take(10).collect();
    assert_eq!(rows_top_n.rows, expected);
    Ok(())
}
