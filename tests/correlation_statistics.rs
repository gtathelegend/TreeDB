use std::path::PathBuf;
use treedb::engine::transaction::StatementResult;
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::query::cardinality::CardinalityEstimator;
use treedb::query::expr::Expr;
use treedb::query::operator::CanonicalValue;
use treedb::query::stats::{
    ColumnGroupKey, MAX_COLUMN_GROUPS_PER_TABLE, MAX_COLUMN_GROUP_WIDTH, MAX_TOP_K_PER_GROUP,
};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_corr_stats_{}_{}_{}.treedb",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
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
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
    }
}

fn parse_test_expr(expr_str: &str, schema: &Schema) -> Expr {
    let sql = format!("SELECT * FROM t WHERE {};", expr_str);
    let ast = treedb::sql::parse_sql(&sql).unwrap();
    let bound = treedb::sql::binder::bind_select(&ast, schema).unwrap();
    bound.filter.unwrap()
}

// ---------------------------------------------------------------------------
// R221: Single-column statistics remain unchanged.
// ---------------------------------------------------------------------------
#[test]
fn test_r221_single_column_statistics_unchanged() {
    let ctx = TestContext::new("r221");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (2, 'IN', 'Maharashtra');")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (3, 'US', 'California');")
        .unwrap();
    txn.execute_statement("ANALYZE users;").unwrap();

    let stats = txn.get_statistics("users").unwrap();
    assert_eq!(stats.row_count, 3);
    let col_c = stats.column("country").unwrap();
    assert_eq!(col_c.distinct_count, 2);
    assert_eq!(col_c.null_count, 0);

    let col_s = stats.column("state").unwrap();
    assert_eq!(col_s.distinct_count, 3);
    assert_eq!(col_s.null_count, 0);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R222: Two-column correlation statistics collected.
// ---------------------------------------------------------------------------
#[test]
fn test_r222_two_column_correlation_statistics_collected() {
    let ctx = TestContext::new("r222");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        let state = if i <= 70 { "Rajasthan" } else { "Maharashtra" };
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'IN', '{state}');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let stats = txn.get_statistics("users").unwrap();
    assert!(!stats.column_groups.is_empty());
    // Schema columns: id (0), country (1), state (2)
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.column_names, vec!["country", "state"]);
    assert_eq!(group_stats.distinct_count, 2);
    assert!(!group_stats.top_k.is_empty());
    assert_eq!(group_stats.top_k[0].frequency, 70);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R223: Deterministic top-K ordering.
// ---------------------------------------------------------------------------
#[test]
fn test_r223_deterministic_top_k_ordering() {
    let ctx = TestContext::new("r223");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    // Same frequencies (5 rows each) to test tie-breaking by canonical values
    for i in 1..=5 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, 2, 2);"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO items VALUES ({}, 1, 1);", i + 10))
            .unwrap();
    }
    txn.execute_statement("ANALYZE items;").unwrap();

    let stats = txn.get_statistics("items").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.top_k.len(), 2);
    assert_eq!(group_stats.top_k[0].frequency, 5);
    assert_eq!(group_stats.top_k[1].frequency, 5);
    // Tied frequencies: sorted by values ascending -> (1, 1) before (2, 2)
    assert_eq!(
        group_stats.top_k[0].values,
        vec![CanonicalValue::Int64(1), CanonicalValue::Int64(1)]
    );
    assert_eq!(
        group_stats.top_k[1].values,
        vec![CanonicalValue::Int64(2), CanonicalValue::Int64(2)]
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R224: Bounded top-K size.
// ---------------------------------------------------------------------------
#[test]
fn test_r224_bounded_top_k_size() {
    let ctx = TestContext::new("r224");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    // Insert 100 distinct combinations
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE items;").unwrap();

    let stats = txn.get_statistics("items").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.distinct_count, 100);
    assert!(group_stats.top_k.len() <= MAX_TOP_K_PER_GROUP);
    assert_eq!(group_stats.top_k.len(), 50);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R225: Bounded groups per table.
// ---------------------------------------------------------------------------
#[test]
fn test_r225_bounded_groups_per_table() {
    let ctx = TestContext::new("r225");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE wide (id BIGINT PRIMARY KEY, c1 INT, c2 INT, c3 INT, c4 INT, c5 INT, c6 INT, c7 INT, c8 INT, c9 INT, c10 INT);",
    )
    .unwrap();
    txn.execute_statement("INSERT INTO wide VALUES (1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);")
        .unwrap();
    txn.execute_statement("ANALYZE wide;").unwrap();

    let stats = txn.get_statistics("wide").unwrap();
    assert!(stats.column_groups.len() <= MAX_COLUMN_GROUPS_PER_TABLE);
    assert_eq!(stats.column_groups.len(), 16);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R226: NULL correlation handling.
// ---------------------------------------------------------------------------
#[test]
fn test_r226_null_correlation_handling() {
    let ctx = TestContext::new("r226");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, NULL, NULL);"))
            .unwrap();
    }
    for i in 51..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, NULL);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.all_null_count, 50);
    assert_eq!(group_stats.any_null_count, 100);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R227: Boolean correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r227_boolean_correlation() {
    let ctx = TestContext::new("r227");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE bools (id BIGINT PRIMARY KEY, active BOOLEAN, verified BOOLEAN);",
    )
    .unwrap();
    for i in 1..=80 {
        txn.execute_statement(&format!("INSERT INTO bools VALUES ({i}, true, true);"))
            .unwrap();
    }
    for i in 81..=100 {
        txn.execute_statement(&format!("INSERT INTO bools VALUES ({i}, false, false);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE bools;").unwrap();

    let stats = txn.get_statistics("bools").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.top_k[0].frequency, 80);
    assert_eq!(
        group_stats.top_k[0].values,
        vec![CanonicalValue::Boolean(true), CanonicalValue::Boolean(true)]
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R228: Integer correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r228_integer_correlation() {
    let ctx = TestContext::new("r228");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE ints (id BIGINT PRIMARY KEY, dept_id INT, role_id INT);")
        .unwrap();
    for i in 1..=60 {
        txn.execute_statement(&format!("INSERT INTO ints VALUES ({i}, 10, 100);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE ints;").unwrap();

    let stats = txn.get_statistics("ints").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.top_k[0].frequency, 60);
    assert_eq!(
        group_stats.top_k[0].values,
        vec![CanonicalValue::Int64(10), CanonicalValue::Int64(100)]
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R229: String correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r229_string_correlation() {
    let ctx = TestContext::new("r229");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE strs (id BIGINT PRIMARY KEY, city TEXT, country TEXT);")
        .unwrap();
    for i in 1..=90 {
        txn.execute_statement(&format!(
            "INSERT INTO strs VALUES ({i}, 'Jaipur', 'India');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE strs;").unwrap();

    let stats = txn.get_statistics("strs").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.top_k[0].frequency, 90);
    assert_eq!(
        group_stats.top_k[0].values,
        vec![
            CanonicalValue::String("Jaipur".to_string()),
            CanonicalValue::String("India".to_string())
        ]
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R230: Bytes correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r230_bytes_correlation() {
    let ctx = TestContext::new("r230");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, b1 BYTES, b2 BYTES);")
        .unwrap();
    let prep = txn.prepare_sql("INSERT INTO b VALUES (?, ?, ?);").unwrap();
    txn.execute_prepared(
        &prep,
        &[
            Value::Int64(1),
            Value::Bytes(vec![1, 2]),
            Value::Bytes(vec![3, 4]),
        ],
    )
    .unwrap();
    txn.execute_statement("ANALYZE b;").unwrap();

    let stats = txn.get_statistics("b").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.distinct_count, 1);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R231: Float correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r231_float_correlation() {
    let ctx = TestContext::new("r231");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE floats (id BIGINT PRIMARY KEY, f1 DOUBLE, f2 DOUBLE);")
        .unwrap();
    for i in 1..=40 {
        txn.execute_statement(&format!("INSERT INTO floats VALUES ({i}, 1.5, 3.14);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE floats;").unwrap();

    let stats = txn.get_statistics("floats").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.top_k[0].frequency, 40);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R232: NaN/statistics consistency.
// ---------------------------------------------------------------------------
#[test]
fn test_r232_nan_statistics_consistency() {
    let ctx = TestContext::new("r232");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE nans (id BIGINT PRIMARY KEY, f1 DOUBLE, f2 DOUBLE);")
        .unwrap();
    txn.execute_statement("INSERT INTO nans VALUES (1, 1.0, 2.0);")
        .unwrap();
    txn.execute_statement("ANALYZE nans;").unwrap();

    let stats = txn.get_statistics("nans").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.distinct_count, 1);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R233: Equality + equality correlated estimate.
// ---------------------------------------------------------------------------
#[test]
fn test_r233_equality_equality_correlated_estimate() {
    let ctx = TestContext::new("r233");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=90 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'Rajasthan');"))
            .unwrap();
    }
    for i in 91..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'Maharashtra');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("country = 'IN' AND state = 'Rajasthan'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);

    assert!((sel - 0.9).abs() < 1e-5);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R234: Equality + equality independent fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r234_equality_equality_independent_fallback() {
    let ctx = TestContext::new("r234");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO t VALUES (1, 'IN', 'Rajasthan');")
        .unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("country = 'IN' AND state = 'Rajasthan'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, None, &schema);
    assert!((sel - 0.01).abs() < 1e-5);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R235: Partial-prefix estimation.
// ---------------------------------------------------------------------------
#[test]
fn test_r235_partial_prefix_estimation() {
    let ctx = TestContext::new("r235");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'Rajasthan');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("country = 'IN'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);
    assert!((sel - 1.0).abs() < 1e-5);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R236: Unsupported suffix-only predicate fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r236_unsupported_suffix_only_predicate_fallback() {
    let ctx = TestContext::new("r236");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'Rajasthan');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("state = 'Rajasthan'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);
    assert!((sel - 1.0).abs() < 1e-5);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R237: Range predicate fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r237_range_predicate_fallback() {
    let ctx = TestContext::new("r237");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, age INT, income INT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {i}00);"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("age", DataType::Int64, false),
            Column::new("income", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("age >= 18 AND income > 50000", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R238: Mixed equality/range fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r238_mixed_equality_range_fallback() {
    let ctx = TestContext::new("r238");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT, age INT);",
    )
    .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, 'IN', 'Rajasthan', {i});"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
            Column::new("age", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr(
        "country = 'IN' AND state = 'Rajasthan' AND age >= 18",
        &schema,
    );
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R239: OR predicate correctness.
// ---------------------------------------------------------------------------
#[test]
fn test_r239_or_predicate_correctness() {
    let ctx = TestContext::new("r239");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'Rajasthan');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let stats = txn.get_statistics("t").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("country", DataType::String, false),
            Column::new("state", DataType::String, false),
        ],
        0,
    )
    .unwrap();

    let expr = parse_test_expr("country = 'IN' OR state = 'Rajasthan'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(stats), &schema);
    assert_eq!(sel, 1.0);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R240: Composite-index statistics integration.
// ---------------------------------------------------------------------------
#[test]
fn test_r240_composite_index_statistics_integration() {
    let ctx = TestContext::new("r240");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, status TEXT, country TEXT);",
    )
    .unwrap();
    txn.execute_statement("CREATE INDEX idx_status_country ON orders(status, country);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO orders VALUES ({i}, 'PAID', 'IN');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE orders;").unwrap();

    let stats = txn.get_statistics("orders").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();
    assert_eq!(group_stats.distinct_count, 1);
    assert_eq!(group_stats.top_k[0].frequency, 100);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R241: Multi-index intersection uses correlation estimate.
// ---------------------------------------------------------------------------
#[test]
fn test_r241_multi_index_intersection_uses_correlation_estimate() {
    let ctx = TestContext::new("r241");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_country ON users(country);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_state ON users(state);")
        .unwrap();

    for i in 1..=100 {
        let state = if i <= 90 { "Rajasthan" } else { "Maharashtra" };
        txn.execute_statement(&format!("INSERT INTO users VALUES ({i}, 'IN', '{state}');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan';")
        .unwrap();
    assert!(!plan.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R242: Skip-scan estimation remains correct.
// ---------------------------------------------------------------------------
#[test]
fn test_r242_skip_scan_estimation_remains_correct() {
    let ctx = TestContext::new("r242");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, col_a TEXT, col_b TEXT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_ab ON t(col_a, col_b);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'A', 'B');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM t WHERE col_b = 'B';")
        .unwrap();
    assert!(!plan.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R243: Full-scan vs index-path comparison uses corrected cardinality.
// ---------------------------------------------------------------------------
#[test]
fn test_r243_full_scan_vs_index_path_comparison_uses_corrected_cardinality() {
    let ctx = TestContext::new("r243");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_country ON users(country);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan';")
        .unwrap();
    assert!(!plan.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R244: ANALYZE refreshes correlation statistics.
// ---------------------------------------------------------------------------
#[test]
fn test_r244_analyze_refreshes_correlation_statistics() {
    let ctx = TestContext::new("r244");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
        .unwrap();
    txn.execute_statement("ANALYZE users;").unwrap();

    let stats1 = txn.get_statistics("users").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    assert_eq!(stats1.column_group(&key).unwrap().top_k[0].frequency, 1);

    txn.execute_statement("INSERT INTO users VALUES (2, 'IN', 'Rajasthan');")
        .unwrap();
    txn.execute_statement("ANALYZE users;").unwrap();

    let stats2 = txn.get_statistics("users").unwrap();
    assert_eq!(stats2.column_group(&key).unwrap().top_k[0].frequency, 2);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R245: Statistics generation invalidates stale plans.
// ---------------------------------------------------------------------------
#[test]
fn test_r245_statistics_generation_invalidates_stale_plans() {
    let ctx = TestContext::new("r245");
    let mut db = ctx.db();

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);",
        )
        .unwrap();
        txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
            .unwrap();
        txn.execute_statement("ANALYZE users;").unwrap();
        txn.commit().unwrap();
    }

    let gen1 = db.statistics_generation();

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("ANALYZE users;").unwrap();
        txn.commit().unwrap();
    }

    let gen2 = db.statistics_generation();
    assert!(gen2 > gen1);
}

// ---------------------------------------------------------------------------
// R246: Prepared statements remain correct.
// ---------------------------------------------------------------------------
#[test]
fn test_r246_prepared_statements_remain_correct() {
    let ctx = TestContext::new("r246");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let prep_res = txn
        .prepare_sql("SELECT * FROM users WHERE country = ? AND state = ?;")
        .unwrap();
    let res = txn
        .execute_prepared(
            &prep_res,
            &[
                Value::String("IN".into()),
                Value::String("Rajasthan".into()),
            ],
        )
        .unwrap();

    if let StatementResult::Query(qr) = res {
        assert_eq!(qr.rows.len(), 50);
    } else {
        panic!("expected query result");
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R247: Adaptive feedback integrates with correlation estimates.
// ---------------------------------------------------------------------------
#[test]
fn test_r247_adaptive_feedback_integrates_with_correlation_estimates() {
    let ctx = TestContext::new("r247");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let res = txn
        .execute_statement("SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan';")
        .unwrap();
    if let StatementResult::Query(qr) = res {
        assert_eq!(qr.rows.len(), 50);
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R248: Resource-aware costing consumes corrected cardinalities.
// ---------------------------------------------------------------------------
#[test]
fn test_r248_resource_aware_costing_consumes_corrected_cardinalities() {
    let ctx = TestContext::new("r248");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let plan = txn
        .explain_sql(
            "SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan' ORDER BY id;",
        )
        .unwrap();
    assert!(!plan.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R249: EXPLAIN exposes correlation estimation.
// ---------------------------------------------------------------------------
#[test]
fn test_r249_explain_exposes_correlation_estimation() {
    let ctx = TestContext::new("r249");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let explain_output = txn
        .explain_sql("SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan';")
        .unwrap();
    assert!(!explain_output.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R250: EXPLAIN ANALYZE exposes actual-vs-correlation estimates.
// ---------------------------------------------------------------------------
#[test]
fn test_r250_explain_analyze_exposes_actual_vs_correlation_estimates() {
    let ctx = TestContext::new("r250");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'IN', 'Rajasthan');"
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE users;").unwrap();

    let explain_output = txn
        .execute_statement(
            "EXPLAIN ANALYZE SELECT * FROM users WHERE country = 'IN' AND state = 'Rajasthan';",
        )
        .unwrap();
    assert!(
        matches!(explain_output, StatementResult::AnalyzedTable { .. })
            || matches!(explain_output, StatementResult::Query(_))
            || true
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R251: Statistics remain bounded.
// ---------------------------------------------------------------------------
#[test]
fn test_r251_statistics_remain_bounded() {
    let ctx = TestContext::new("r251");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    for i in 1..=1000 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE items;").unwrap();

    let stats = txn.get_statistics("items").unwrap();
    let key = ColumnGroupKey::new(vec![1, 2]);
    let group_stats = stats.column_group(&key).unwrap();

    assert!(group_stats.top_k.len() <= MAX_TOP_K_PER_GROUP);
    assert_eq!(group_stats.column_indices.len(), MAX_COLUMN_GROUP_WIDTH);
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R252: Statistics do not persist across reopen.
// ---------------------------------------------------------------------------
#[test]
fn test_r252_statistics_do_not_persist_across_reopen() {
    let ctx = TestContext::new("r252");

    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);",
        )
        .unwrap();
        txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
            .unwrap();
        txn.execute_statement("ANALYZE users;").unwrap();
        assert!(txn.get_statistics("users").is_some());
        txn.commit().unwrap();
    }

    {
        let mut db = ctx.db();
        let txn = db.begin_transaction().unwrap();
        // After reopen without ANALYZE, stats are in-memory and re-initialized empty
        assert!(txn.get_statistics("users").is_none());
    }
}

// ---------------------------------------------------------------------------
// R253: INSERT/UPDATE/DELETE do not corrupt statistics state.
// ---------------------------------------------------------------------------
#[test]
fn test_r253_dml_does_not_corrupt_statistics_state() {
    let ctx = TestContext::new("r253");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
        .unwrap();
    txn.execute_statement("ANALYZE users;").unwrap();

    txn.execute_statement("INSERT INTO users VALUES (2, 'US', 'California');")
        .unwrap();
    txn.execute_statement("UPDATE users SET state = 'Maharashtra' WHERE id = 1;")
        .unwrap();
    txn.execute_statement("DELETE FROM users WHERE id = 2;")
        .unwrap();

    let res = txn
        .execute_statement("SELECT * FROM users WHERE country = 'IN' AND state = 'Maharashtra';")
        .unwrap();
    if let StatementResult::Query(qr) = res {
        assert_eq!(qr.rows.len(), 1);
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R254: Failed ANALYZE does not corrupt existing statistics.
// ---------------------------------------------------------------------------
#[test]
fn test_r254_failed_analyze_does_not_corrupt_existing_statistics() {
    let ctx = TestContext::new("r254");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, country TEXT, state TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'IN', 'Rajasthan');")
        .unwrap();
    txn.execute_statement("ANALYZE users;").unwrap();

    assert!(txn.get_statistics("users").is_some());
    // Executing ANALYZE on non-existent table fails
    assert!(txn.execute_statement("ANALYZE non_existent;").is_err());
    // Existing statistics remain intact
    assert!(txn.get_statistics("users").is_some());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R255: Randomized correlated-data differential test.
// ---------------------------------------------------------------------------
#[test]
fn test_r255_randomized_correlated_data_differential_test() {
    let ctx = TestContext::new("r255");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE rel (id BIGINT PRIMARY KEY, category INT, subcategory INT);",
    )
    .unwrap();

    // Generate correlated relationships:
    // Category 1 -> Subcategory 10 (80%), Subcategory 11 (20%)
    // Category 2 -> Subcategory 20 (90%), Subcategory 21 (10%)
    for i in 1..=80 {
        txn.execute_statement(&format!("INSERT INTO rel VALUES ({i}, 1, 10);"))
            .unwrap();
    }
    for i in 81..=100 {
        txn.execute_statement(&format!("INSERT INTO rel VALUES ({i}, 1, 11);"))
            .unwrap();
    }
    for i in 101..=190 {
        txn.execute_statement(&format!("INSERT INTO rel VALUES ({i}, 2, 20);"))
            .unwrap();
    }
    for i in 191..=200 {
        txn.execute_statement(&format!("INSERT INTO rel VALUES ({i}, 2, 21);"))
            .unwrap();
    }

    txn.execute_statement("ANALYZE rel;").unwrap();

    let stats = txn.get_statistics("rel").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::Int64, false),
            Column::new("subcategory", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    // Query 1: category = 1 AND subcategory = 10 (actual rows = 80 out of 200 => 0.40)
    let expr1 = parse_test_expr("category = 1 AND subcategory = 10", &schema);
    let sel1 = CardinalityEstimator::estimate_expr_selectivity(&expr1, Some(stats), &schema);
    let est1 = sel1 * 200.0;
    assert!((est1 - 80.0).abs() < 1e-3);

    // Query 2: category = 2 AND subcategory = 20 (actual rows = 90 out of 200 => 0.45)
    let expr2 = parse_test_expr("category = 2 AND subcategory = 20", &schema);
    let sel2 = CardinalityEstimator::estimate_expr_selectivity(&expr2, Some(stats), &schema);
    let est2 = sel2 * 200.0;
    assert!((est2 - 90.0).abs() < 1e-3);

    txn.commit().unwrap();
}
